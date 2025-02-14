use anyhow::Result;
use enum_as_inner::EnumAsInner;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::{collections::HashMap, fmt::Debug};

use super::module::{Module, NS_DEFAULT_DB, NS_SELF, NS_STD};
use crate::ast::pl::*;
use crate::ast::rq::RelationColumn;
use crate::error::Span;

/// Context of the pipeline.
#[derive(Default, Serialize, Deserialize, Clone)]
pub struct Context {
    /// Map of all accessible names (for each namespace)
    pub(crate) root_mod: Module,

    pub(crate) span_map: HashMap<usize, Span>,

    pub(crate) inferred_columns: HashMap<usize, Vec<RelationColumn>>,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct Decl {
    pub declared_at: Option<usize>,

    pub kind: DeclKind,
}

#[derive(Debug, Serialize, Deserialize, Clone, EnumAsInner)]
pub enum DeclKind {
    /// A nested namespace
    Module(Module),

    /// Nested namespaces that do lookup in layers from top to bottom, stopping at first match.
    LayeredModules(Vec<Module>),

    TableDecl(TableDecl),

    InstanceOf(Ident),

    Column(usize),

    /// Contains a default value to be created in parent namespace matched.
    Wildcard(Box<DeclKind>),

    FuncDef(FuncDef),

    Expr(Box<Expr>),

    NoResolve,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TableDecl {
    /// Columns layout
    pub columns: Vec<RelationColumn>,

    /// None means that this is an extern table (actual table in database)
    /// Some means a CTE
    pub expr: Option<Box<Expr>>,
}

#[derive(Clone, Eq, Debug, PartialEq, Serialize, Deserialize)]
pub enum TableColumn {
    Wildcard,
    Single(Option<String>),
}

impl Context {
    pub fn declare_func(&mut self, func_def: FuncDef, id: Option<usize>) {
        let name = func_def.name.clone();

        let path = vec![NS_STD.to_string()];
        let ident = Ident { name, path };

        let decl = Decl {
            kind: DeclKind::FuncDef(func_def),
            declared_at: id,
        };
        self.root_mod.insert(ident, decl).unwrap();
    }

    pub fn declare_table(&mut self, table_def: TableDef, id: Option<usize>) {
        let name = table_def.name;
        let path = vec![NS_DEFAULT_DB.to_string()];
        let ident = Ident { name, path };

        let frame = table_def.value.ty.clone().unwrap().into_table().unwrap();
        let columns = (frame.columns.into_iter())
            .map(|col| match col {
                FrameColumn::Wildcard { .. } => RelationColumn::Wildcard,
                FrameColumn::Single { name, .. } => RelationColumn::Single(name.map(|n| n.name)),
            })
            .collect();

        let expr = Some(table_def.value);
        let decl = Decl {
            declared_at: id,
            kind: DeclKind::TableDecl(TableDecl { columns, expr }),
        };

        self.root_mod.insert(ident, decl).unwrap();
    }

    pub fn resolve_ident(&mut self, ident: &Ident) -> Result<Ident, String> {
        // lookup the name
        let decls = self.root_mod.lookup(ident);

        match decls.len() {
            // no match: try match *
            0 => {}

            // single match, great!
            1 => return Ok(decls.into_iter().next().unwrap()),

            // ambiguous
            _ => {
                let decls = decls.into_iter().map(|d| d.to_string()).join(", ");
                return Err(format!("Ambiguous name. Could be from any of {decls}"));
            }
        }

        // this variable can be from a namespace that we don't know all columns of
        let decls = if ident.name != "*" {
            self.root_mod.lookup(&Ident {
                path: ident.path.clone(),
                name: "*".to_string(),
            })
        } else {
            HashSet::new()
        };

        match decls.len() {
            0 => Err(format!("Unknown name {ident}")),

            // single match, great!
            1 => {
                let wildcard_ident = decls.into_iter().next().unwrap();

                let wildcard = self.root_mod.get(&wildcard_ident).unwrap();
                let wildcard_default = wildcard.kind.as_wildcard().cloned().unwrap();
                let input_id = wildcard.declared_at;

                let module_ident = wildcard_ident.pop().unwrap();
                let module = self.root_mod.get_mut(&module_ident).unwrap();
                let module = module.kind.as_module_mut().unwrap();

                // insert default
                module
                    .names
                    .insert(ident.name.clone(), Decl::from(*wildcard_default));

                // infer table columns
                if let Some(decl) = module.names.get(NS_SELF).cloned() {
                    if let DeclKind::InstanceOf(table_ident) = decl.kind {
                        log::debug!("inferring {ident} to be from table {table_ident}");
                        self.infer_table_column(&table_ident, &ident.name)?;
                    }
                }

                // for inline expressions with wildcards (s-strings), we cannot store inferred columns
                // in global namespace, but still need the information for lowering.
                // as a workaround, we store it in context directly.
                if let Some(input_id) = input_id {
                    let inferred = self.inferred_columns.entry(input_id).or_default();

                    let exists = inferred.iter().any(|c| match c {
                        RelationColumn::Single(Some(name)) => name == &ident.name,
                        _ => false,
                    });
                    if !exists {
                        inferred.push(RelationColumn::Single(Some(ident.name.clone())));
                    }
                }

                Ok(module_ident + Ident::from_name(ident.name.clone()))
            }

            // ambiguous
            _ => {
                let decls = decls.into_iter().map(|d| d.to_string()).join(", ");
                Err(format!("Ambiguous name. Could be from any of {decls}"))
            }
        }
    }

    fn infer_table_column(&mut self, table_ident: &Ident, col_name: &str) -> Result<(), String> {
        let table = self.root_mod.get_mut(table_ident).unwrap();
        let table_decl = table.kind.as_table_decl_mut().unwrap();

        let has_wildcard =
            (table_decl.columns.iter()).any(|c| matches!(c, RelationColumn::Wildcard));
        if !has_wildcard {
            return Err(format!("Table {table_ident:?} does not have wildcard."));
        }

        let exists = table_decl.columns.iter().any(|c| match c {
            RelationColumn::Single(Some(n)) => n == col_name,
            _ => false,
        });
        if exists {
            return Ok(());
        }

        let col = RelationColumn::Single(Some(col_name.to_string()));
        table_decl.columns.push(col);

        // also add into input tables of this table expression
        if let Some(expr) = &table_decl.expr {
            if let Some(Ty::Table(frame)) = expr.ty.as_ref() {
                let wildcard_inputs = (frame.columns.iter())
                    .filter_map(|c| c.as_wildcard())
                    .collect_vec();

                match wildcard_inputs.len() {
                    0 => return Err(format!("Cannot infer where {table_ident}.{col_name} is from")),
                    1 => {
                        let input_name = wildcard_inputs.into_iter().next().unwrap();

                        let input = frame.find_input(input_name).unwrap();
                        if let Some(table_ident) = input.table.clone() {
                            self.infer_table_column(&table_ident, col_name)?;
                        }
                    }
                    _ => {
                        return Err(format!("Cannot infer where {table_ident}.{col_name} is from. It could be any of {wildcard_inputs:?}"))
                    }
                }
            }
        }

        Ok(())
    }
}

impl Default for DeclKind {
    fn default() -> Self {
        DeclKind::Module(Module::default())
    }
}

impl Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.root_mod.fmt(f)
    }
}

impl From<DeclKind> for Decl {
    fn from(kind: DeclKind) -> Self {
        Decl {
            kind,
            declared_at: None,
        }
    }
}

impl std::fmt::Display for Decl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.kind, f)
    }
}

impl std::fmt::Display for DeclKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Module(arg0) => f.debug_tuple("Module").field(arg0).finish(),
            Self::LayeredModules(arg0) => f.debug_tuple("LayeredModules").field(arg0).finish(),
            Self::TableDecl(TableDecl { columns, expr }) => {
                write!(f, "TableDef: {} {expr:?}", RelationColumns(columns))
            }
            Self::InstanceOf(arg0) => write!(f, "InstanceOf: {arg0}"),
            Self::Column(arg0) => write!(f, "Column (target {arg0})"),
            Self::Wildcard(arg0) => write!(f, "Wildcard (default: {arg0})"),
            Self::FuncDef(arg0) => write!(f, "FuncDef: {arg0}"),
            Self::Expr(arg0) => write!(f, "Expr: {arg0}"),
            Self::NoResolve => write!(f, "NoResolve"),
        }
    }
}

pub struct RelationColumns<'a>(pub &'a [RelationColumn]);

impl<'a> std::fmt::Display for RelationColumns<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[")?;
        for (index, col) in self.0.iter().enumerate() {
            let is_last = index == self.0.len() - 1;

            let col = match col {
                RelationColumn::Wildcard => "*",
                RelationColumn::Single(name) => name.as_deref().unwrap_or("<unnamed>"),
            };
            f.write_str(col)?;
            if !is_last {
                f.write_str(", ")?;
            }
        }
        write!(f, "]")
    }
}
