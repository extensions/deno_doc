// Copyright 2020-2021 the Deno authors. All rights reserved. MIT license.
use crate::swc_util::AstParser;
use crate::ReexportKind;
use deno_graph::ModuleGraph;
use deno_graph::ModuleSpecifier;
use swc_common::comments::CommentKind;
use swc_common::Span;
use swc_ecmascript::ast::Decl;
use swc_ecmascript::ast::DefaultDecl;
use swc_ecmascript::ast::ExportSpecifier;
use swc_ecmascript::ast::Expr;
use swc_ecmascript::ast::ImportSpecifier;
use swc_ecmascript::ast::ModuleDecl;
use swc_ecmascript::ast::ModuleItem;
use swc_ecmascript::ast::Stmt;
use swc_ecmascript::parser::Syntax;

use crate::namespace::NamespaceDef;
use crate::node;
use crate::node::DocNode;
use crate::node::ModuleDoc;
use crate::swc_util;
use crate::ImportDef;
use crate::Location;
use regex::Regex;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;

#[derive(Debug)]
pub enum DocError {
  Resolve(String),
  Io(std::io::Error),
  Parse(swc_util::SwcDiagnosticBuffer),
}

impl Error for DocError {}

impl fmt::Display for DocError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let m = match self {
      Self::Resolve(s) => s.to_string(),
      Self::Io(err) => err.to_string(),
      Self::Parse(err) => err.to_string(),
    };
    f.pad(&m)
  }
}

impl From<swc_util::SwcDiagnosticBuffer> for DocError {
  fn from(error: swc_util::SwcDiagnosticBuffer) -> DocError {
    DocError::Parse(error)
  }
}

#[derive(Clone)]
enum ImportKind {
  Namespace(String),
  Named(String, Option<String>),
}

#[derive(Clone)]
struct Import {
  src: String,
  kind: ImportKind,
}

pub struct DocParser {
  pub ast_parser: AstParser,
  pub graph: ModuleGraph,
  pub private: bool,
}

impl DocParser {
  pub fn new(graph: ModuleGraph, private: bool) -> Self {
    DocParser {
      ast_parser: AstParser::default(),
      graph,
      private,
    }
  }

  /// Parses a module into a list of exported items,
  /// as well as a list of reexported items which need to be fetched from other modules.
  pub fn parse_module(
    &self,
    specifier: &ModuleSpecifier,
    syntax: Syntax,
    source_code: &str,
  ) -> Result<ModuleDoc, DocError> {
    let parse_result =
      self
        .ast_parser
        .parse_module(&specifier.to_string(), syntax, source_code);
    let module = parse_result?;
    let mut doc_entries =
      self.get_doc_nodes_for_module_body(module.body.clone());
    let import_doc_entries =
      self.get_doc_nodes_for_module_imports(module.body.clone(), specifier)?;
    doc_entries.extend(import_doc_entries);
    let reexports = self.get_reexports_for_module_body(module.body);
    let module_doc = ModuleDoc {
      definitions: doc_entries,
      reexports,
    };
    Ok(module_doc)
  }

  /// Fetches `file_name` and parses it.
  pub fn parse(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Result<Vec<DocNode>, DocError> {
    let module = self
      .graph
      .try_get(specifier)
      .map_err(|err| DocError::Resolve(err.to_string()))?
      .ok_or_else(|| DocError::Resolve(specifier.to_string()))?;

    self.parse_source(
      &module.specifier,
      Syntax::from(&module.media_type),
      &module.source,
    )
  }

  /// Parses a module and returns a list of exported items (no reexports).
  pub fn parse_source(
    &self,
    specifier: &ModuleSpecifier,
    syntax: Syntax,
    source_code: &str,
  ) -> Result<Vec<DocNode>, DocError> {
    self
      .parse_module(specifier, syntax, source_code)
      .map(|md| md.definitions)
  }

  fn flatten_reexports(
    &self,
    reexports: &[node::Reexport],
    referrer: &ModuleSpecifier,
  ) -> Result<Vec<DocNode>, DocError> {
    let mut by_src: HashMap<String, Vec<node::Reexport>> = HashMap::new();

    let mut processed_reexports: Vec<DocNode> = vec![];

    for reexport in reexports {
      if by_src.get(&reexport.src).is_none() {
        by_src.insert(reexport.src.to_string(), vec![]);
      }

      let bucket = by_src.get_mut(&reexport.src).unwrap();
      bucket.push(reexport.clone());
    }

    for specifier in by_src.keys() {
      let resolved_specifier = self
        .graph
        .resolve_dependency(specifier, referrer)
        .ok_or_else(|| DocError::Resolve(specifier.clone()))?;
      let doc_nodes = self.parse_with_reexports(resolved_specifier)?;
      let reexports_for_specifier = by_src.get(specifier).unwrap();

      for reexport in reexports_for_specifier {
        match &reexport.kind {
          node::ReexportKind::All => {
            processed_reexports.extend(doc_nodes.clone())
          }
          node::ReexportKind::Namespace(ns_name) => {
            let ns_def = NamespaceDef {
              elements: doc_nodes.clone(),
            };
            let ns_doc_node = DocNode::namespace(
              ns_name.to_string(),
              Location {
                filename: specifier.to_string(),
                line: 1,
                col: 0,
              },
              None,
              ns_def,
            );
            processed_reexports.push(ns_doc_node);
          }
          node::ReexportKind::Named(ident, maybe_alias) => {
            // Try to find reexport.
            // NOTE: the reexport might actually be reexport from another
            // module; for now we're skipping nested reexports.
            let doc_nodes = doc_nodes
              .iter()
              .filter(|node| &node.name == ident)
              .collect::<Vec<_>>();

            for doc_node in doc_nodes {
              let doc_node = doc_node.clone();
              let doc_node = if let Some(alias) = maybe_alias {
                DocNode {
                  name: alias.to_string(),
                  ..doc_node
                }
              } else {
                doc_node
              };

              processed_reexports.push(doc_node);
            }
          }
        }
      }
    }

    Ok(processed_reexports)
  }

  /// Fetches `file_name`, parses it, and resolves its reexports.
  pub fn parse_with_reexports(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Result<Vec<DocNode>, DocError> {
    let module = self
      .graph
      .try_get(specifier)
      .map_err(|err| DocError::Resolve(err.to_string()))?
      .ok_or_else(|| DocError::Resolve(specifier.to_string()))?;

    let module_doc = self.parse_module(
      &module.specifier,
      Syntax::from(&module.media_type),
      &module.source,
    )?;

    let flattened_docs = if !module_doc.reexports.is_empty() {
      let mut flattenned_reexports =
        self.flatten_reexports(&module_doc.reexports, &module.specifier)?;
      flattenned_reexports.extend(module_doc.definitions);
      flattenned_reexports
    } else {
      module_doc.definitions
    };

    Ok(flattened_docs)
  }

  fn get_doc_nodes_for_module_imports(
    &self,
    module_body: Vec<swc_ecmascript::ast::ModuleItem>,
    referrer: &ModuleSpecifier,
  ) -> Result<Vec<DocNode>, DocError> {
    let mut imports = vec![];

    for node in module_body.iter() {
      if let swc_ecmascript::ast::ModuleItem::ModuleDecl(ModuleDecl::Import(
        import_decl,
      )) = node
      {
        let (js_doc, location) = self.details_for_span(import_decl.span);
        for specifier in &import_decl.specifiers {
          use swc_ecmascript::ast::ImportSpecifier::*;

          let (name, maybe_imported_name, src) = match specifier {
            Named(named_specifier) => (
              named_specifier.local.sym.to_string(),
              named_specifier
                .imported
                .as_ref()
                .map(|ident| ident.sym.to_string())
                .or_else(|| Some(named_specifier.local.sym.to_string())),
              import_decl.src.value.to_string(),
            ),
            Default(default_specifier) => (
              default_specifier.local.sym.to_string(),
              Some("default".to_string()),
              import_decl.src.value.to_string(),
            ),
            Namespace(namespace_specifier) => (
              namespace_specifier.local.sym.to_string(),
              None,
              import_decl.src.value.to_string(),
            ),
          };

          let resolved_specifier = self
            .graph
            .resolve_dependency(&src, referrer)
            .ok_or_else(|| DocError::Resolve(src.clone()))?;
          let import_def = ImportDef {
            src: resolved_specifier.to_string(),
            imported: maybe_imported_name,
          };

          let doc_node =
            DocNode::import(name, location.clone(), js_doc.clone(), import_def);

          imports.push(doc_node);
        }
      }
    }

    Ok(imports)
  }

  pub fn get_doc_nodes_for_module_exports(
    &self,
    module_decl: &ModuleDecl,
  ) -> Vec<DocNode> {
    match module_decl {
      ModuleDecl::ExportDecl(export_decl) => {
        vec![super::module::get_doc_node_for_export_decl(
          self,
          export_decl,
        )]
      }
      ModuleDecl::ExportDefaultDecl(export_default_decl) => {
        let (js_doc, location) =
          self.details_for_span(export_default_decl.span);
        let name = "default".to_string();

        let doc_node = match &export_default_decl.decl {
          DefaultDecl::Class(class_expr) => {
            let class_def =
              crate::class::class_to_class_def(self, &class_expr.class);
            DocNode::class(name, location, js_doc, class_def)
          }
          DefaultDecl::Fn(fn_expr) => {
            let function_def = crate::function::function_to_function_def(
              self,
              &fn_expr.function,
            );
            DocNode::function(name, location, js_doc, function_def)
          }
          DefaultDecl::TsInterfaceDecl(interface_decl) => {
            let (_, interface_def) =
              crate::interface::get_doc_for_ts_interface_decl(
                self,
                interface_decl,
              );
            DocNode::interface(name, location, js_doc, interface_def)
          }
        };

        vec![doc_node]
      }
      _ => vec![],
    }
  }

  fn details_for_span(&self, span: Span) -> (Option<String>, Location) {
    let js_doc = self.js_doc_for_span(span);
    let location = self.ast_parser.get_span_location(span).into();
    (js_doc, location)
  }

  pub fn get_doc_node_for_decl(&self, decl: &Decl) -> Option<DocNode> {
    match decl {
      Decl::Class(class_decl) => {
        let (name, class_def) =
          super::class::get_doc_for_class_decl(self, class_decl);
        let (js_doc, location) = self.details_for_span(class_decl.class.span);
        Some(DocNode::class(name, location, js_doc, class_def))
      }
      Decl::Fn(fn_decl) => {
        let (name, function_def) =
          super::function::get_doc_for_fn_decl(self, fn_decl);
        let (js_doc, location) = self.details_for_span(fn_decl.function.span);
        Some(DocNode::function(name, location, js_doc, function_def))
      }
      Decl::Var(var_decl) => {
        let (name, var_def) = super::variable::get_doc_for_var_decl(var_decl);
        let (js_doc, location) = self.details_for_span(var_decl.span);
        Some(DocNode::variable(name, location, js_doc, var_def))
      }
      Decl::TsInterface(ts_interface_decl) => {
        let (name, interface_def) =
          super::interface::get_doc_for_ts_interface_decl(
            self,
            ts_interface_decl,
          );
        let (js_doc, location) = self.details_for_span(ts_interface_decl.span);
        Some(DocNode::interface(name, location, js_doc, interface_def))
      }
      Decl::TsTypeAlias(ts_type_alias) => {
        let (name, type_alias_def) =
          super::type_alias::get_doc_for_ts_type_alias_decl(
            self,
            ts_type_alias,
          );
        let (js_doc, location) = self.details_for_span(ts_type_alias.span);
        Some(DocNode::type_alias(name, location, js_doc, type_alias_def))
      }
      Decl::TsEnum(ts_enum) => {
        let (name, enum_def) =
          super::r#enum::get_doc_for_ts_enum_decl(self, ts_enum);
        let (js_doc, location) = self.details_for_span(ts_enum.span);
        Some(DocNode::r#enum(name, location, js_doc, enum_def))
      }
      Decl::TsModule(ts_module) => {
        let (name, namespace_def) =
          super::namespace::get_doc_for_ts_module(self, ts_module);
        let (js_doc, location) = self.details_for_span(ts_module.span);
        Some(DocNode::namespace(name, location, js_doc, namespace_def))
      }
    }
  }

  fn get_imports_for_module_body(
    &self,
    module_body: &[swc_ecmascript::ast::ModuleItem],
  ) -> HashMap<String, Import> {
    let mut imports = HashMap::new();

    for node in module_body.iter() {
      if let ModuleItem::ModuleDecl(ModuleDecl::Import(import_decl)) = node {
        for specifier in &import_decl.specifiers {
          let import = match specifier {
            ImportSpecifier::Named(named_specifier) => Import {
              kind: ImportKind::Named(
                named_specifier.local.sym.to_string(),
                named_specifier
                  .imported
                  .as_ref()
                  .map(|ident| ident.sym.to_string()),
              ),
              src: import_decl.src.value.to_string(),
            },
            ImportSpecifier::Default(default_specifier) => Import {
              kind: ImportKind::Named(
                default_specifier.local.sym.to_string(),
                Some("default".to_string()),
              ),
              src: import_decl.src.value.to_string(),
            },
            ImportSpecifier::Namespace(namespace_specifier) => Import {
              kind: ImportKind::Namespace(
                namespace_specifier.local.sym.to_string(),
              ),
              src: import_decl.src.value.to_string(),
            },
          };

          let name = match import.kind.clone() {
            ImportKind::Named(name, _) | ImportKind::Namespace(name) => name,
          };

          imports.insert(name, import);
        }
      }
    }

    imports
  }

  pub fn get_reexports_for_module_body(
    &self,
    module_body: Vec<swc_ecmascript::ast::ModuleItem>,
  ) -> Vec<node::Reexport> {
    let imports = self.get_imports_for_module_body(&module_body);

    let mut reexports: Vec<node::Reexport> = vec![];

    if self.private {
      reexports.extend(imports.values().cloned().map(|import| node::Reexport {
        src: import.src,
        kind: match import.kind {
          ImportKind::Named(orig, exported) => {
            ReexportKind::Named(orig, exported)
          }
          ImportKind::Namespace(name) => ReexportKind::Namespace(name),
        },
      }))
    }

    for node in module_body.iter() {
      if let swc_ecmascript::ast::ModuleItem::ModuleDecl(module_decl) = node {
        let r = match module_decl {
          ModuleDecl::ExportNamed(named_export) => {
            if let Some(src) = &named_export.src {
              let src_str = src.value.to_string();
              named_export
                .specifiers
                .iter()
                .map(|export_specifier| match export_specifier {
                  ExportSpecifier::Namespace(ns_export) => node::Reexport {
                    kind: node::ReexportKind::Namespace(
                      ns_export.name.sym.to_string(),
                    ),
                    src: src_str.to_string(),
                  },
                  ExportSpecifier::Default(specifier) => node::Reexport {
                    kind: node::ReexportKind::Named(
                      "default".to_string(),
                      Some(specifier.exported.sym.to_string()),
                    ),
                    src: src_str.to_string(),
                  },
                  ExportSpecifier::Named(named_export) => {
                    let ident = named_export.orig.sym.to_string();
                    let maybe_alias =
                      named_export.exported.as_ref().map(|e| e.sym.to_string());
                    let kind = node::ReexportKind::Named(ident, maybe_alias);
                    node::Reexport {
                      kind,
                      src: src_str.to_string(),
                    }
                  }
                })
                .collect::<Vec<node::Reexport>>()
            } else {
              named_export
                .specifiers
                .iter()
                .filter_map(|specifier| {
                  if let ExportSpecifier::Named(specifier) = specifier {
                    if let Some(import) =
                      imports.get(&specifier.orig.sym.to_string())
                    {
                      // If it has the same name as the original import and private values are exported,
                      // don't export this again and document the same value twice.
                      if self.private && specifier.exported.is_none() {
                        return None;
                      }

                      let name = specifier
                        .exported
                        .as_ref()
                        .unwrap_or(&specifier.orig)
                        .sym
                        .to_string();
                      Some(node::Reexport {
                        src: import.src.clone(),
                        kind: match &import.kind {
                          ImportKind::Named(orig, _) => {
                            ReexportKind::Named(orig.clone(), Some(name))
                          }
                          ImportKind::Namespace(_) => {
                            ReexportKind::Namespace(name)
                          }
                        },
                      })
                    } else {
                      None
                    }
                  } else {
                    None
                  }
                })
                .collect()
            }
          }
          ModuleDecl::ExportAll(export_all) => {
            let reexport = node::Reexport {
              kind: node::ReexportKind::All,
              src: export_all.src.value.to_string(),
            };
            vec![reexport]
          }
          _ => vec![],
        };

        reexports.extend(r);
      }
    }

    reexports
  }

  fn get_symbols_for_module_body(
    &self,
    module_body: &[swc_ecmascript::ast::ModuleItem],
  ) -> HashMap<String, DocNode> {
    let mut symbols = HashMap::new();

    for node in module_body.iter() {
      let doc_node = match node {
        ModuleItem::Stmt(Stmt::Decl(decl)) => self.get_doc_node_for_decl(decl),
        ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export_decl)) => Some(
          super::module::get_doc_node_for_export_decl(self, export_decl),
        ),
        _ => None,
      };

      if let Some(doc_node) = doc_node {
        symbols.insert(doc_node.name.clone(), doc_node);
      }
    }

    symbols
  }

  pub fn get_doc_nodes_for_module_body(
    &self,
    module_body: Vec<swc_ecmascript::ast::ModuleItem>,
  ) -> Vec<DocNode> {
    let symbols = self.get_symbols_for_module_body(&module_body);

    let mut doc_entries: Vec<DocNode> = Vec::new();
    let mut ambient_entries: Vec<DocNode> = Vec::new();

    let mut is_ambient = true;

    for node in module_body.iter() {
      match node {
        ModuleItem::Stmt(stmt) => {
          if let Stmt::Decl(decl) = stmt {
            if let Some(doc_node) = self.get_doc_node_for_decl(decl) {
              let is_declared = self.get_declare_for_decl(decl);
              if self.private {
                doc_entries.push(doc_node);
              } else if is_declared {
                ambient_entries.push(doc_node)
              }
            }
          }
        }

        ModuleItem::ModuleDecl(module_decl) => {
          // If it has imports/exports, it isn't ambient.
          is_ambient = false;

          doc_entries
            .extend(self.get_doc_nodes_for_module_exports(module_decl));

          match module_decl {
            ModuleDecl::ExportNamed(export_named) => {
              for specifier in &export_named.specifiers {
                match specifier {
                  ExportSpecifier::Named(named_specifier) => {
                    // If it has the same name as the original symbol and private values are exported,
                    // don't export this again and document the same value twice.
                    if self.private && named_specifier.exported.is_none() {
                      continue;
                    }

                    let symbol = named_specifier.orig.sym.to_string();
                    if let Some(doc_node) = symbols.get(&symbol) {
                      let mut doc_node = doc_node.clone();
                      if let Some(exported) = &named_specifier.exported {
                        doc_node.name = exported.sym.to_string()
                      }
                      doc_entries.push(doc_node)
                    }
                  }
                  // TODO(zhmushan)
                  ExportSpecifier::Default(_default_specifier) => {}
                  ExportSpecifier::Namespace(_namespace_specifier) => {}
                }
              }
            }
            ModuleDecl::ExportDefaultExpr(export_expr) => {
              if let Expr::Ident(ident) = export_expr.expr.as_ref() {
                if let Some(doc_node) = symbols.get(&ident.sym.to_string()) {
                  doc_entries.push(DocNode {
                    name: String::from("default"),
                    ..doc_node.clone()
                  });
                }
              } else {
                let (js_doc, location) =
                  self.details_for_span(export_expr.span);
                doc_entries.push(DocNode::variable(
                  String::from("default"),
                  location,
                  js_doc,
                  super::variable::VariableDef {
                    kind: swc_ecmascript::ast::VarDeclKind::Var,
                    ts_type: super::ts_type::infer_simple_ts_type_from_expr(
                      export_expr.expr.as_ref(),
                      true,
                    ),
                  },
                ));
              }
            }
            _ => {}
          }
        }
      }
    }

    if is_ambient {
      doc_entries.extend(ambient_entries);
    }

    doc_entries
  }

  pub fn js_doc_for_span(&self, span: Span) -> Option<String> {
    let comments = self.ast_parser.get_span_comments(span);
    let js_doc_comment = comments.iter().rev().find(|comment| {
      comment.kind == CommentKind::Block && comment.text.starts_with('*')
    })?;

    let mut margin_pat = String::from("");
    if let Some(margin) = self.ast_parser.source_map.span_to_margin(span) {
      for _ in 0..margin {
        margin_pat.push(' ');
      }
    }

    let js_doc_re = Regex::new(r#" ?\* ?"#).unwrap();
    let txt = js_doc_comment
      .text
      .split('\n')
      .map(|line| js_doc_re.replace(line, "").to_string())
      .map(|line| {
        if line.starts_with(&margin_pat) {
          line[margin_pat.len()..].to_string()
        } else {
          line
        }
      })
      .collect::<Vec<String>>()
      .join("\n");

    let txt = txt.trim_start().trim_end().to_string();

    Some(txt)
  }

  fn get_declare_for_decl(&self, decl: &Decl) -> bool {
    match decl {
      Decl::Class(class_decl) => class_decl.declare,
      Decl::Fn(fn_decl) => fn_decl.declare,
      Decl::TsEnum(ts_enum_decl) => ts_enum_decl.declare,
      Decl::TsInterface(ts_interface_decl) => ts_interface_decl.declare,
      Decl::TsModule(ts_module_decl) => ts_module_decl.declare,
      Decl::TsTypeAlias(ts_type_alias_decl) => ts_type_alias_decl.declare,
      Decl::Var(var_decl) => var_decl.declare,
    }
  }
}
