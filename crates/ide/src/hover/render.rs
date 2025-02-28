//! Logic for rendering the different hover messages
use std::fmt::Display;

use either::Either;
use hir::{AsAssocItem, HasAttrs, HasSource, HirDisplay, Semantics, TypeInfo};
use ide_db::{
    base_db::SourceDatabase,
    defs::Definition,
    helpers::{
        generated_lints::{CLIPPY_LINTS, DEFAULT_LINTS, FEATURES},
        FamousDefs,
    },
    RootDatabase,
};
use itertools::Itertools;
use stdx::format_to;
use syntax::{
    algo, ast,
    display::{fn_as_proc_macro_label, macro_label},
    match_ast, AstNode, Direction,
    SyntaxKind::{CONDITION, LET_STMT},
    SyntaxToken, T,
};

use crate::{
    doc_links::{remove_links, rewrite_links},
    hover::walk_and_push_ty,
    markdown_remove::remove_markdown,
    HoverAction, HoverConfig, HoverResult, Markup,
};

pub(super) fn type_info(
    sema: &Semantics<RootDatabase>,
    config: &HoverConfig,
    expr_or_pat: &Either<ast::Expr, ast::Pat>,
) -> Option<HoverResult> {
    let TypeInfo { original, adjusted } = match expr_or_pat {
        Either::Left(expr) => sema.type_of_expr(expr)?,
        Either::Right(pat) => sema.type_of_pat(pat)?,
    };

    let mut res = HoverResult::default();
    let mut targets: Vec<hir::ModuleDef> = Vec::new();
    let mut push_new_def = |item: hir::ModuleDef| {
        if !targets.contains(&item) {
            targets.push(item);
        }
    };
    walk_and_push_ty(sema.db, &original, &mut push_new_def);

    res.markup = if let Some(adjusted_ty) = adjusted {
        walk_and_push_ty(sema.db, &adjusted_ty, &mut push_new_def);
        let original = original.display(sema.db).to_string();
        let adjusted = adjusted_ty.display(sema.db).to_string();
        let static_text_diff_len = "Coerced to: ".len() - "Type: ".len();
        format!(
            "{bt_start}Type: {:>apad$}\nCoerced to: {:>opad$}\n{bt_end}",
            original,
            adjusted,
            apad = static_text_diff_len + adjusted.len().max(original.len()),
            opad = original.len(),
            bt_start = if config.markdown() { "```text\n" } else { "" },
            bt_end = if config.markdown() { "```\n" } else { "" }
        )
        .into()
    } else {
        if config.markdown() {
            Markup::fenced_block(&original.display(sema.db))
        } else {
            original.display(sema.db).to_string().into()
        }
    };
    res.actions.push(HoverAction::goto_type_from_targets(sema.db, targets));
    Some(res)
}

pub(super) fn try_expr(
    sema: &Semantics<RootDatabase>,
    config: &HoverConfig,
    try_expr: &ast::TryExpr,
) -> Option<HoverResult> {
    let inner_ty = sema.type_of_expr(&try_expr.expr()?)?.original;
    let mut ancestors = try_expr.syntax().ancestors();
    let mut body_ty = loop {
        let next = ancestors.next()?;
        break match_ast! {
            match next {
                ast::Fn(fn_) => sema.to_def(&fn_)?.ret_type(sema.db),
                ast::Item(__) => return None,
                ast::ClosureExpr(closure) => sema.type_of_expr(&closure.body()?)?.original,
                ast::BlockExpr(block_expr) => if matches!(block_expr.modifier(), Some(ast::BlockModifier::Async(_) | ast::BlockModifier::Try(_)| ast::BlockModifier::Const(_))) {
                    sema.type_of_expr(&block_expr.into())?.original
                } else {
                    continue;
                },
                _ => continue,
            }
        };
    };

    if inner_ty == body_ty {
        return None;
    }

    let mut inner_ty = inner_ty;
    let mut s = "Try Target".to_owned();

    let adts = inner_ty.as_adt().zip(body_ty.as_adt());
    if let Some((hir::Adt::Enum(inner), hir::Adt::Enum(body))) = adts {
        let famous_defs = FamousDefs(sema, sema.scope(&try_expr.syntax()).krate());
        // special case for two options, there is no value in showing them
        if let Some(option_enum) = famous_defs.core_option_Option() {
            if inner == option_enum && body == option_enum {
                cov_mark::hit!(hover_try_expr_opt_opt);
                return None;
            }
        }

        // special case two results to show the error variants only
        if let Some(result_enum) = famous_defs.core_result_Result() {
            if inner == result_enum && body == result_enum {
                let error_type_args =
                    inner_ty.type_arguments().nth(1).zip(body_ty.type_arguments().nth(1));
                if let Some((inner, body)) = error_type_args {
                    inner_ty = inner;
                    body_ty = body;
                    s = "Try Error".to_owned();
                }
            }
        }
    }

    let mut res = HoverResult::default();

    let mut targets: Vec<hir::ModuleDef> = Vec::new();
    let mut push_new_def = |item: hir::ModuleDef| {
        if !targets.contains(&item) {
            targets.push(item);
        }
    };
    walk_and_push_ty(sema.db, &inner_ty, &mut push_new_def);
    walk_and_push_ty(sema.db, &body_ty, &mut push_new_def);
    res.actions.push(HoverAction::goto_type_from_targets(sema.db, targets));

    let inner_ty = inner_ty.display(sema.db).to_string();
    let body_ty = body_ty.display(sema.db).to_string();
    let ty_len_max = inner_ty.len().max(body_ty.len());

    let l = "Propagated as: ".len() - " Type: ".len();
    let static_text_len_diff = l as isize - s.len() as isize;
    let tpad = static_text_len_diff.max(0) as usize;
    let ppad = static_text_len_diff.min(0).abs() as usize;

    res.markup = format!(
        "{bt_start}{} Type: {:>pad0$}\nPropagated as: {:>pad1$}\n{bt_end}",
        s,
        inner_ty,
        body_ty,
        pad0 = ty_len_max + tpad,
        pad1 = ty_len_max + ppad,
        bt_start = if config.markdown() { "```text\n" } else { "" },
        bt_end = if config.markdown() { "```\n" } else { "" }
    )
    .into();
    Some(res)
}

pub(super) fn deref_expr(
    sema: &Semantics<RootDatabase>,
    config: &HoverConfig,
    deref_expr: &ast::PrefixExpr,
) -> Option<HoverResult> {
    let inner_ty = sema.type_of_expr(&deref_expr.expr()?)?.original;
    let TypeInfo { original, adjusted } =
        sema.type_of_expr(&ast::Expr::from(deref_expr.clone()))?;

    let mut res = HoverResult::default();
    let mut targets: Vec<hir::ModuleDef> = Vec::new();
    let mut push_new_def = |item: hir::ModuleDef| {
        if !targets.contains(&item) {
            targets.push(item);
        }
    };
    walk_and_push_ty(sema.db, &inner_ty, &mut push_new_def);
    walk_and_push_ty(sema.db, &original, &mut push_new_def);

    res.markup = if let Some(adjusted_ty) = adjusted {
        walk_and_push_ty(sema.db, &adjusted_ty, &mut push_new_def);
        let original = original.display(sema.db).to_string();
        let adjusted = adjusted_ty.display(sema.db).to_string();
        let inner = inner_ty.display(sema.db).to_string();
        let type_len = "To type: ".len();
        let coerced_len = "Coerced to: ".len();
        let deref_len = "Dereferenced from: ".len();
        let max_len = (original.len() + type_len)
            .max(adjusted.len() + coerced_len)
            .max(inner.len() + deref_len);
        format!(
            "{bt_start}Dereferenced from: {:>ipad$}\nTo type: {:>apad$}\nCoerced to: {:>opad$}\n{bt_end}",
            inner,
            original,
            adjusted,
            ipad = max_len - deref_len,
            apad = max_len - type_len,
            opad = max_len - coerced_len,
            bt_start = if config.markdown() { "```text\n" } else { "" },
            bt_end = if config.markdown() { "```\n" } else { "" }
        )
        .into()
    } else {
        let original = original.display(sema.db).to_string();
        let inner = inner_ty.display(sema.db).to_string();
        let type_len = "To type: ".len();
        let deref_len = "Dereferenced from: ".len();
        let max_len = (original.len() + type_len).max(inner.len() + deref_len);
        format!(
            "{bt_start}Dereferenced from: {:>ipad$}\nTo type: {:>apad$}\n{bt_end}",
            inner,
            original,
            ipad = max_len - deref_len,
            apad = max_len - type_len,
            bt_start = if config.markdown() { "```text\n" } else { "" },
            bt_end = if config.markdown() { "```\n" } else { "" }
        )
        .into()
    };
    res.actions.push(HoverAction::goto_type_from_targets(sema.db, targets));

    Some(res)
}

pub(super) fn keyword(
    sema: &Semantics<RootDatabase>,
    config: &HoverConfig,
    token: &SyntaxToken,
) -> Option<HoverResult> {
    if !token.kind().is_keyword() || !config.documentation.is_some() {
        return None;
    }
    let famous_defs = FamousDefs(sema, sema.scope(&token.parent()?).krate());
    // std exposes {}_keyword modules with docstrings on the root to document keywords
    let keyword_mod = format!("{}_keyword", token.text());
    let doc_owner = find_std_module(&famous_defs, &keyword_mod)?;
    let docs = doc_owner.attrs(sema.db).docs()?;
    let markup = process_markup(
        sema.db,
        Definition::Module(doc_owner),
        &markup(Some(docs.into()), token.text().into(), None)?,
        config,
    );
    Some(HoverResult { markup, actions: Default::default() })
}

pub(super) fn try_for_lint(attr: &ast::Attr, token: &SyntaxToken) -> Option<HoverResult> {
    let (path, tt) = attr.as_simple_call()?;
    if !tt.syntax().text_range().contains(token.text_range().start()) {
        return None;
    }
    let (is_clippy, lints) = match &*path {
        "feature" => (false, FEATURES),
        "allow" | "deny" | "forbid" | "warn" => {
            let is_clippy = algo::non_trivia_sibling(token.clone().into(), Direction::Prev)
                .filter(|t| t.kind() == T![:])
                .and_then(|t| algo::non_trivia_sibling(t, Direction::Prev))
                .filter(|t| t.kind() == T![:])
                .and_then(|t| algo::non_trivia_sibling(t, Direction::Prev))
                .map_or(false, |t| {
                    t.kind() == T![ident] && t.into_token().map_or(false, |t| t.text() == "clippy")
                });
            if is_clippy {
                (true, CLIPPY_LINTS)
            } else {
                (false, DEFAULT_LINTS)
            }
        }
        _ => return None,
    };

    let tmp;
    let needle = if is_clippy {
        tmp = format!("clippy::{}", token.text());
        &tmp
    } else {
        &*token.text()
    };

    let lint =
        lints.binary_search_by_key(&needle, |lint| lint.label).ok().map(|idx| &lints[idx])?;
    Some(HoverResult {
        markup: Markup::from(format!("```\n{}\n```\n___\n\n{}", lint.label, lint.description)),
        ..Default::default()
    })
}

pub(super) fn process_markup(
    db: &RootDatabase,
    def: Definition,
    markup: &Markup,
    config: &HoverConfig,
) -> Markup {
    let markup = markup.as_str();
    let markup = if !config.markdown() {
        remove_markdown(markup)
    } else if config.links_in_hover {
        rewrite_links(db, markup, def)
    } else {
        remove_links(markup)
    };
    Markup::from(markup)
}

fn definition_owner_name(db: &RootDatabase, def: &Definition) -> Option<String> {
    match def {
        Definition::Field(f) => Some(f.parent_def(db).name(db)),
        Definition::Local(l) => l.parent(db).name(db),
        Definition::Function(f) => match f.as_assoc_item(db)?.container(db) {
            hir::AssocItemContainer::Trait(t) => Some(t.name(db)),
            hir::AssocItemContainer::Impl(i) => i.self_ty(db).as_adt().map(|adt| adt.name(db)),
        },
        Definition::Variant(e) => Some(e.parent_enum(db).name(db)),
        _ => None,
    }
    .map(|name| name.to_string())
}

pub(super) fn path(db: &RootDatabase, module: hir::Module, item_name: Option<String>) -> String {
    let crate_name =
        db.crate_graph()[module.krate().into()].display_name.as_ref().map(|it| it.to_string());
    let module_path = module
        .path_to_root(db)
        .into_iter()
        .rev()
        .flat_map(|it| it.name(db).map(|name| name.to_string()));
    crate_name.into_iter().chain(module_path).chain(item_name).join("::")
}

pub(super) fn definition(
    db: &RootDatabase,
    def: Definition,
    famous_defs: Option<&FamousDefs>,
    config: &HoverConfig,
) -> Option<Markup> {
    let mod_path = definition_mod_path(db, &def);
    let (label, docs) = match def {
        Definition::Macro(it) => (
            match &it.source(db)?.value {
                Either::Left(mac) => macro_label(mac),
                Either::Right(mac_fn) => fn_as_proc_macro_label(mac_fn),
            },
            it.attrs(db).docs(),
        ),
        Definition::Field(def) => label_and_docs(db, def),
        Definition::Module(it) => label_and_docs(db, it),
        Definition::Function(it) => label_and_docs(db, it),
        Definition::Adt(it) => label_and_docs(db, it),
        Definition::Variant(it) => label_and_docs(db, it),
        Definition::Const(it) => label_value_and_docs(db, it, |it| it.value(db)),
        Definition::Static(it) => label_value_and_docs(db, it, |it| it.value(db)),
        Definition::Trait(it) => label_and_docs(db, it),
        Definition::TypeAlias(it) => label_and_docs(db, it),
        Definition::BuiltinType(it) => {
            return famous_defs
                .and_then(|fd| builtin(fd, it))
                .or_else(|| Some(Markup::fenced_block(&it.name())))
        }
        Definition::Local(it) => return local(db, it),
        Definition::SelfType(impl_def) => {
            impl_def.self_ty(db).as_adt().map(|adt| label_and_docs(db, adt))?
        }
        Definition::GenericParam(it) => label_and_docs(db, it),
        Definition::Label(it) => return Some(Markup::fenced_block(&it.name(db))),
    };

    markup(docs.filter(|_| config.documentation.is_some()).map(Into::into), label, mod_path)
}

fn label_and_docs<D>(db: &RootDatabase, def: D) -> (String, Option<hir::Documentation>)
where
    D: HasAttrs + HirDisplay,
{
    let label = def.display(db).to_string();
    let docs = def.attrs(db).docs();
    (label, docs)
}

fn label_value_and_docs<D, E, V>(
    db: &RootDatabase,
    def: D,
    value_extractor: E,
) -> (String, Option<hir::Documentation>)
where
    D: HasAttrs + HirDisplay,
    E: Fn(&D) -> Option<V>,
    V: Display,
{
    let label = if let Some(value) = (value_extractor)(&def) {
        format!("{} = {}", def.display(db), value)
    } else {
        def.display(db).to_string()
    };
    let docs = def.attrs(db).docs();
    (label, docs)
}

fn definition_mod_path(db: &RootDatabase, def: &Definition) -> Option<String> {
    if let Definition::GenericParam(_) = def {
        return None;
    }
    def.module(db).map(|module| path(db, module, definition_owner_name(db, def)))
}

fn markup(docs: Option<String>, desc: String, mod_path: Option<String>) -> Option<Markup> {
    let mut buf = String::new();

    if let Some(mod_path) = mod_path {
        if !mod_path.is_empty() {
            format_to!(buf, "```rust\n{}\n```\n\n", mod_path);
        }
    }
    format_to!(buf, "```rust\n{}\n```", desc);

    if let Some(doc) = docs {
        format_to!(buf, "\n___\n\n{}", doc);
    }
    Some(buf.into())
}

fn builtin(famous_defs: &FamousDefs, builtin: hir::BuiltinType) -> Option<Markup> {
    // std exposes prim_{} modules with docstrings on the root to document the builtins
    let primitive_mod = format!("prim_{}", builtin.name());
    let doc_owner = find_std_module(famous_defs, &primitive_mod)?;
    let docs = doc_owner.attrs(famous_defs.0.db).docs()?;
    markup(Some(docs.into()), builtin.name().to_string(), None)
}

fn find_std_module(famous_defs: &FamousDefs, name: &str) -> Option<hir::Module> {
    let db = famous_defs.0.db;
    let std_crate = famous_defs.std()?;
    let std_root_module = std_crate.root_module(db);
    std_root_module
        .children(db)
        .find(|module| module.name(db).map_or(false, |module| module.to_string() == name))
}

fn local(db: &RootDatabase, it: hir::Local) -> Option<Markup> {
    let ty = it.ty(db);
    let ty = ty.display_truncated(db, None);
    let is_mut = if it.is_mut(db) { "mut " } else { "" };
    let desc = match it.source(db).value {
        Either::Left(ident) => {
            let name = it.name(db).unwrap();
            let let_kw = if ident
                .syntax()
                .parent()
                .map_or(false, |p| p.kind() == LET_STMT || p.kind() == CONDITION)
            {
                "let "
            } else {
                ""
            };
            format!("{}{}{}: {}", let_kw, is_mut, name, ty)
        }
        Either::Right(_) => format!("{}self: {}", is_mut, ty),
    };
    markup(None, desc, None)
}
