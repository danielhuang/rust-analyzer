//! See `CompletionContext` structure.

use std::iter;

use base_db::SourceDatabaseExt;
use hir::{
    HasAttrs, Local, Name, PathResolution, ScopeDef, Semantics, SemanticsScope, Type, TypeInfo,
};
use ide_db::{
    active_parameter::ActiveParameter,
    base_db::{FilePosition, SourceDatabase},
    famous_defs::FamousDefs,
    FxHashMap, FxHashSet, RootDatabase,
};
use syntax::{
    algo::{find_node_at_offset, non_trivia_sibling},
    ast::{self, AttrKind, HasArgList, HasName, NameOrNameRef},
    match_ast, AstNode, AstToken, NodeOrToken,
    SyntaxKind::{self, *},
    SyntaxNode, SyntaxToken, TextRange, TextSize, T,
};
use text_edit::Indel;

use crate::{
    patterns::{
        determine_location, determine_prev_sibling, is_in_loop_body, is_in_token_of_for_loop,
        previous_token, ImmediateLocation, ImmediatePrevSibling,
    },
    CompletionConfig,
};

const COMPLETION_MARKER: &str = "intellijRulezz";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum PatternRefutability {
    Refutable,
    Irrefutable,
}

pub(crate) enum Visible {
    Yes,
    Editable,
    No,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum PathKind {
    Expr {
        in_block_expr: bool,
        in_loop_body: bool,
        in_functional_update: bool,
    },
    Type,
    Attr {
        kind: AttrKind,
        annotated_item_kind: Option<SyntaxKind>,
    },
    Derive,
    /// Path in item position, that is inside an (Assoc)ItemList
    Item {
        kind: ItemListKind,
    },
    Pat,
    Vis {
        has_in_token: bool,
    },
    Use,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum ItemListKind {
    SourceFile,
    Module,
    Impl,
    Trait,
    ExternBlock,
}

#[derive(Debug)]
pub(crate) struct PathCompletionCtx {
    /// If this is a call with () already there (or {} in case of record patterns)
    pub(super) has_call_parens: bool,
    /// If this has a macro call bang !
    pub(super) has_macro_bang: bool,
    /// Whether this path stars with a `::`.
    pub(super) is_absolute_path: bool,
    /// The qualifier of the current path if it exists.
    pub(super) qualifier: Option<PathQualifierCtx>,
    #[allow(dead_code)]
    // FIXME: use this
    /// The parent of the path we are completing.
    pub(super) parent: Option<ast::Path>,
    pub(super) kind: PathKind,
    /// Whether the path segment has type args or not.
    pub(super) has_type_args: bool,
}

#[derive(Debug)]
pub(crate) struct PathQualifierCtx {
    pub(crate) path: ast::Path,
    pub(crate) resolution: Option<PathResolution>,
    /// Whether this path consists solely of `super` segments
    pub(crate) is_super_chain: bool,
    /// Whether the qualifier comes from a use tree parent or not
    pub(crate) use_tree_parent: bool,
    /// <_>
    pub(crate) is_infer_qualifier: bool,
}

#[derive(Debug)]
pub(super) struct PatternContext {
    pub(super) refutability: PatternRefutability,
    pub(super) param_ctx: Option<(ast::ParamList, ast::Param, ParamKind)>,
    pub(super) has_type_ascription: bool,
    pub(super) parent_pat: Option<ast::Pat>,
    pub(super) ref_token: Option<SyntaxToken>,
    pub(super) mut_token: Option<SyntaxToken>,
}

#[derive(Debug)]
pub(super) struct LifetimeContext {
    pub(super) lifetime: Option<ast::Lifetime>,
    pub(super) kind: LifetimeKind,
}

#[derive(Debug)]
pub(super) enum LifetimeKind {
    LifetimeParam { is_decl: bool, param: ast::LifetimeParam },
    Lifetime,
    LabelRef,
    LabelDef,
}

#[derive(Debug)]
pub(super) struct NameContext {
    #[allow(dead_code)]
    pub(super) name: Option<ast::Name>,
    pub(super) kind: NameKind,
}

#[derive(Debug)]
#[allow(dead_code)]
pub(super) enum NameKind {
    Const,
    ConstParam,
    Enum,
    Function,
    IdentPat,
    MacroDef,
    MacroRules,
    /// Fake node
    Module(ast::Module),
    RecordField,
    Rename,
    SelfParam,
    Static,
    Struct,
    Trait,
    TypeAlias,
    TypeParam,
    Union,
    Variant,
}

#[derive(Debug)]
pub(super) struct NameRefContext {
    /// NameRef syntax in the original file
    pub(super) nameref: Option<ast::NameRef>,
    pub(super) dot_access: Option<DotAccess>,
    pub(super) path_ctx: Option<PathCompletionCtx>,
}

#[derive(Debug)]
pub(super) enum IdentContext {
    Name(NameContext),
    NameRef(NameRefContext),
    Lifetime(LifetimeContext),
    /// Original token, fake token
    String {
        original: ast::String,
        expanded: Option<ast::String>,
    },
    UnexpandedAttrTT {
        fake_attribute_under_caret: Option<ast::Attr>,
    },
}

#[derive(Debug)]
pub(super) enum DotAccess {
    Field {
        receiver: Option<ast::Expr>,
        /// True if the receiver is an integer and there is no ident in the original file after it yet
        /// like `0.$0`
        receiver_is_ambiguous_float_literal: bool,
    },
    Method {
        receiver: Option<ast::Expr>,
        has_parens: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ParamKind {
    Function(ast::Fn),
    Closure(ast::ClosureExpr),
}

/// `CompletionContext` is created early during completion to figure out, where
/// exactly is the cursor, syntax-wise.
#[derive(Debug)]
pub(crate) struct CompletionContext<'a> {
    pub(super) sema: Semantics<'a, RootDatabase>,
    pub(super) scope: SemanticsScope<'a>,
    pub(super) db: &'a RootDatabase,
    pub(super) config: &'a CompletionConfig,
    pub(super) position: FilePosition,

    /// The token before the cursor, in the original file.
    pub(super) original_token: SyntaxToken,
    /// The token before the cursor, in the macro-expanded file.
    pub(super) token: SyntaxToken,
    /// The crate of the current file.
    pub(super) krate: hir::Crate,
    /// The module of the `scope`.
    pub(super) module: hir::Module,

    /// The expected name of what we are completing.
    /// This is usually the parameter name of the function argument we are completing.
    pub(super) expected_name: Option<NameOrNameRef>,
    /// The expected type of what we are completing.
    pub(super) expected_type: Option<Type>,

    /// The parent function of the cursor position if it exists.
    pub(super) function_def: Option<ast::Fn>,
    /// The parent impl of the cursor position if it exists.
    pub(super) impl_def: Option<ast::Impl>,
    /// Are we completing inside a let statement with a missing semicolon?
    pub(super) incomplete_let: bool,

    pub(super) completion_location: Option<ImmediateLocation>,
    pub(super) prev_sibling: Option<ImmediatePrevSibling>,
    pub(super) previous_token: Option<SyntaxToken>,

    pub(super) ident_ctx: IdentContext,

    pub(super) pattern_ctx: Option<PatternContext>,

    pub(super) existing_derives: FxHashSet<hir::Macro>,

    pub(super) locals: FxHashMap<Name, Local>,
}

impl<'a> CompletionContext<'a> {
    /// The range of the identifier that is being completed.
    pub(crate) fn source_range(&self) -> TextRange {
        // check kind of macro-expanded token, but use range of original token
        let kind = self.token.kind();
        match kind {
            CHAR => {
                // assume we are completing a lifetime but the user has only typed the '
                cov_mark::hit!(completes_if_lifetime_without_idents);
                TextRange::at(self.original_token.text_range().start(), TextSize::from(1))
            }
            IDENT | LIFETIME_IDENT | UNDERSCORE => self.original_token.text_range(),
            _ if kind.is_keyword() => self.original_token.text_range(),
            _ => TextRange::empty(self.position.offset),
        }
    }

    pub(crate) fn previous_token_is(&self, kind: SyntaxKind) -> bool {
        self.previous_token.as_ref().map_or(false, |tok| tok.kind() == kind)
    }

    pub(crate) fn famous_defs(&self) -> FamousDefs {
        FamousDefs(&self.sema, self.krate)
    }

    pub(super) fn nameref_ctx(&self) -> Option<&NameRefContext> {
        match &self.ident_ctx {
            IdentContext::NameRef(it) => Some(it),
            _ => None,
        }
    }

    pub(super) fn name_ctx(&self) -> Option<&NameContext> {
        match &self.ident_ctx {
            IdentContext::Name(it) => Some(it),
            _ => None,
        }
    }

    pub(super) fn lifetime_ctx(&self) -> Option<&LifetimeContext> {
        match &self.ident_ctx {
            IdentContext::Lifetime(it) => Some(it),
            _ => None,
        }
    }

    pub(crate) fn dot_receiver(&self) -> Option<&ast::Expr> {
        match self.nameref_ctx() {
            Some(NameRefContext {
                dot_access:
                    Some(DotAccess::Method { receiver, .. } | DotAccess::Field { receiver, .. }),
                ..
            }) => receiver.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn has_dot_receiver(&self) -> bool {
        self.dot_receiver().is_some()
    }

    pub(crate) fn expects_assoc_item(&self) -> bool {
        matches!(self.completion_location, Some(ImmediateLocation::Trait | ImmediateLocation::Impl))
    }

    pub(crate) fn expects_variant(&self) -> bool {
        matches!(self.name_ctx(), Some(NameContext { kind: NameKind::Variant, .. }))
    }

    pub(crate) fn expects_non_trait_assoc_item(&self) -> bool {
        matches!(self.completion_location, Some(ImmediateLocation::Impl))
    }

    pub(crate) fn expects_item(&self) -> bool {
        matches!(self.completion_location, Some(ImmediateLocation::ItemList))
    }

    // FIXME: This shouldn't exist
    pub(crate) fn expects_generic_arg(&self) -> bool {
        matches!(self.completion_location, Some(ImmediateLocation::GenericArgList(_)))
    }

    pub(crate) fn has_block_expr_parent(&self) -> bool {
        matches!(self.completion_location, Some(ImmediateLocation::StmtList))
    }

    pub(crate) fn expects_ident_ref_expr(&self) -> bool {
        matches!(self.completion_location, Some(ImmediateLocation::RefExpr))
    }

    pub(crate) fn expect_field(&self) -> bool {
        matches!(self.completion_location, Some(ImmediateLocation::TupleField))
            || matches!(self.name_ctx(), Some(NameContext { kind: NameKind::RecordField, .. }))
    }

    /// Whether the cursor is right after a trait or impl header.
    /// trait Foo ident$0
    // FIXME: This probably shouldn't exist
    pub(crate) fn has_unfinished_impl_or_trait_prev_sibling(&self) -> bool {
        matches!(
            self.prev_sibling,
            Some(ImmediatePrevSibling::ImplDefType | ImmediatePrevSibling::TraitDefName)
        )
    }

    // FIXME: This probably shouldn't exist
    pub(crate) fn has_impl_prev_sibling(&self) -> bool {
        matches!(self.prev_sibling, Some(ImmediatePrevSibling::ImplDefType))
    }

    pub(crate) fn has_visibility_prev_sibling(&self) -> bool {
        matches!(self.prev_sibling, Some(ImmediatePrevSibling::Visibility))
    }

    pub(crate) fn after_if(&self) -> bool {
        matches!(self.prev_sibling, Some(ImmediatePrevSibling::IfExpr))
    }

    // FIXME: This shouldn't exist
    pub(crate) fn is_path_disallowed(&self) -> bool {
        self.previous_token_is(T![unsafe])
            || matches!(
                self.prev_sibling,
                Some(ImmediatePrevSibling::Attribute | ImmediatePrevSibling::Visibility)
            )
            || matches!(
                self.completion_location,
                Some(ImmediateLocation::RecordPat(_) | ImmediateLocation::RecordExpr(_))
            )
            || matches!(
                self.name_ctx(),
                Some(NameContext { kind: NameKind::Module(_) | NameKind::Rename, .. })
            )
    }

    pub(crate) fn path_context(&self) -> Option<&PathCompletionCtx> {
        self.nameref_ctx().and_then(|ctx| ctx.path_ctx.as_ref())
    }

    pub(crate) fn expects_expression(&self) -> bool {
        matches!(self.path_context(), Some(PathCompletionCtx { kind: PathKind::Expr { .. }, .. }))
    }

    pub(crate) fn is_non_trivial_path(&self) -> bool {
        matches!(
            self.path_context(),
            Some(
                PathCompletionCtx { is_absolute_path: true, .. }
                    | PathCompletionCtx { qualifier: Some(_), .. }
            )
        )
    }

    pub(crate) fn path_qual(&self) -> Option<&ast::Path> {
        self.path_context().and_then(|it| it.qualifier.as_ref().map(|it| &it.path))
    }

    pub(crate) fn path_kind(&self) -> Option<PathKind> {
        self.path_context().map(|it| it.kind)
    }

    /// Checks if an item is visible and not `doc(hidden)` at the completion site.
    pub(crate) fn is_visible<I>(&self, item: &I) -> Visible
    where
        I: hir::HasVisibility + hir::HasAttrs + hir::HasCrate + Copy,
    {
        self.is_visible_impl(&item.visibility(self.db), &item.attrs(self.db), item.krate(self.db))
    }

    pub(crate) fn is_scope_def_hidden(&self, scope_def: ScopeDef) -> bool {
        if let (Some(attrs), Some(krate)) = (scope_def.attrs(self.db), scope_def.krate(self.db)) {
            return self.is_doc_hidden(&attrs, krate);
        }

        false
    }

    /// Check if an item is `#[doc(hidden)]`.
    pub(crate) fn is_item_hidden(&self, item: &hir::ItemInNs) -> bool {
        let attrs = item.attrs(self.db);
        let krate = item.krate(self.db);
        match (attrs, krate) {
            (Some(attrs), Some(krate)) => self.is_doc_hidden(&attrs, krate),
            _ => false,
        }
    }
    /// Whether the given trait is an operator trait or not.
    pub(crate) fn is_ops_trait(&self, trait_: hir::Trait) -> bool {
        match trait_.attrs(self.db).lang() {
            Some(lang) => OP_TRAIT_LANG_NAMES.contains(&lang.as_str()),
            None => false,
        }
    }

    /// Returns the traits in scope, with the [`Drop`] trait removed.
    pub(crate) fn traits_in_scope(&self) -> hir::VisibleTraits {
        let mut traits_in_scope = self.scope.visible_traits();
        if let Some(drop) = self.famous_defs().core_ops_Drop() {
            traits_in_scope.0.remove(&drop.into());
        }
        traits_in_scope
    }

    /// A version of [`SemanticsScope::process_all_names`] that filters out `#[doc(hidden)]` items.
    pub(crate) fn process_all_names(&self, f: &mut dyn FnMut(Name, ScopeDef)) {
        let _p = profile::span("CompletionContext::process_all_names");
        self.scope.process_all_names(&mut |name, def| {
            if self.is_scope_def_hidden(def) {
                return;
            }

            f(name, def);
        });
    }

    pub(crate) fn process_all_names_raw(&self, f: &mut dyn FnMut(Name, ScopeDef)) {
        let _p = profile::span("CompletionContext::process_all_names_raw");
        self.scope.process_all_names(&mut |name, def| f(name, def));
    }

    fn is_visible_impl(
        &self,
        vis: &hir::Visibility,
        attrs: &hir::Attrs,
        defining_crate: hir::Crate,
    ) -> Visible {
        if !vis.is_visible_from(self.db, self.module.into()) {
            if !self.config.enable_private_editable {
                return Visible::No;
            }
            // If the definition location is editable, also show private items
            let root_file = defining_crate.root_file(self.db);
            let source_root_id = self.db.file_source_root(root_file);
            let is_editable = !self.db.source_root(source_root_id).is_library;
            return if is_editable { Visible::Editable } else { Visible::No };
        }

        if self.is_doc_hidden(attrs, defining_crate) {
            Visible::No
        } else {
            Visible::Yes
        }
    }

    fn is_doc_hidden(&self, attrs: &hir::Attrs, defining_crate: hir::Crate) -> bool {
        // `doc(hidden)` items are only completed within the defining crate.
        self.krate != defining_crate && attrs.has_doc_hidden()
    }
}

// CompletionContext construction
impl<'a> CompletionContext<'a> {
    pub(super) fn new(
        db: &'a RootDatabase,
        position @ FilePosition { file_id, offset }: FilePosition,
        config: &'a CompletionConfig,
    ) -> Option<CompletionContext<'a>> {
        let _p = profile::span("CompletionContext::new");
        let sema = Semantics::new(db);

        let original_file = sema.parse(file_id);

        // Insert a fake ident to get a valid parse tree. We will use this file
        // to determine context, though the original_file will be used for
        // actual completion.
        let file_with_fake_ident = {
            let parse = db.parse(file_id);
            let edit = Indel::insert(offset, COMPLETION_MARKER.to_string());
            parse.reparse(&edit).tree()
        };
        let fake_ident_token =
            file_with_fake_ident.syntax().token_at_offset(offset).right_biased()?;

        let original_token = original_file.syntax().token_at_offset(offset).left_biased()?;
        let token = sema.descend_into_macros_single(original_token.clone());
        let scope = sema.scope_at_offset(&token.parent()?, offset)?;
        let krate = scope.krate();
        let module = scope.module();

        let mut locals = FxHashMap::default();
        scope.process_all_names(&mut |name, scope| {
            if let ScopeDef::Local(local) = scope {
                locals.insert(name, local);
            }
        });

        let mut ctx = CompletionContext {
            sema,
            scope,
            db,
            config,
            position,
            original_token,
            token,
            krate,
            module,
            expected_name: None,
            expected_type: None,
            function_def: None,
            impl_def: None,
            incomplete_let: false,
            completion_location: None,
            prev_sibling: None,
            previous_token: None,
            // dummy value, will be overwritten
            ident_ctx: IdentContext::UnexpandedAttrTT { fake_attribute_under_caret: None },
            pattern_ctx: None,
            existing_derives: Default::default(),
            locals,
        };
        ctx.expand_and_fill(
            original_file.syntax().clone(),
            file_with_fake_ident.syntax().clone(),
            offset,
            fake_ident_token,
        )?;
        Some(ctx)
    }

    /// Expand attributes and macro calls at the current cursor position for both the original file
    /// and fake file repeatedly. As soon as one of the two expansions fail we stop so the original
    /// and speculative states stay in sync.
    fn expand_and_fill(
        &mut self,
        mut original_file: SyntaxNode,
        mut speculative_file: SyntaxNode,
        mut offset: TextSize,
        mut fake_ident_token: SyntaxToken,
    ) -> Option<()> {
        let _p = profile::span("CompletionContext::expand_and_fill");
        let mut derive_ctx = None;

        'expansion: loop {
            let parent_item =
                |item: &ast::Item| item.syntax().ancestors().skip(1).find_map(ast::Item::cast);
            let ancestor_items = iter::successors(
                Option::zip(
                    find_node_at_offset::<ast::Item>(&original_file, offset),
                    find_node_at_offset::<ast::Item>(&speculative_file, offset),
                ),
                |(a, b)| parent_item(a).zip(parent_item(b)),
            );

            // first try to expand attributes as these are always the outermost macro calls
            'ancestors: for (actual_item, item_with_fake_ident) in ancestor_items {
                match (
                    self.sema.expand_attr_macro(&actual_item),
                    self.sema.speculative_expand_attr_macro(
                        &actual_item,
                        &item_with_fake_ident,
                        fake_ident_token.clone(),
                    ),
                ) {
                    // maybe parent items have attributes, so continue walking the ancestors
                    (None, None) => continue 'ancestors,
                    // successful expansions
                    (Some(actual_expansion), Some((fake_expansion, fake_mapped_token))) => {
                        let new_offset = fake_mapped_token.text_range().start();
                        if new_offset > actual_expansion.text_range().end() {
                            // offset outside of bounds from the original expansion,
                            // stop here to prevent problems from happening
                            break 'expansion;
                        }
                        original_file = actual_expansion;
                        speculative_file = fake_expansion;
                        fake_ident_token = fake_mapped_token;
                        offset = new_offset;
                        continue 'expansion;
                    }
                    // exactly one expansion failed, inconsistent state so stop expanding completely
                    _ => break 'expansion,
                }
            }

            // No attributes have been expanded, so look for macro_call! token trees or derive token trees
            let orig_tt = match find_node_at_offset::<ast::TokenTree>(&original_file, offset) {
                Some(it) => it,
                None => break 'expansion,
            };
            let spec_tt = match find_node_at_offset::<ast::TokenTree>(&speculative_file, offset) {
                Some(it) => it,
                None => break 'expansion,
            };

            // Expand pseudo-derive expansion
            if let (Some(orig_attr), Some(spec_attr)) = (
                orig_tt.syntax().parent().and_then(ast::Meta::cast).and_then(|it| it.parent_attr()),
                spec_tt.syntax().parent().and_then(ast::Meta::cast).and_then(|it| it.parent_attr()),
            ) {
                if let (Some(actual_expansion), Some((fake_expansion, fake_mapped_token))) = (
                    self.sema.expand_derive_as_pseudo_attr_macro(&orig_attr),
                    self.sema.speculative_expand_derive_as_pseudo_attr_macro(
                        &orig_attr,
                        &spec_attr,
                        fake_ident_token.clone(),
                    ),
                ) {
                    derive_ctx = Some((
                        actual_expansion,
                        fake_expansion,
                        fake_mapped_token.text_range().start(),
                        orig_attr,
                    ));
                }
                // at this point we won't have any more successful expansions, so stop
                break 'expansion;
            }

            // Expand fn-like macro calls
            if let (Some(actual_macro_call), Some(macro_call_with_fake_ident)) = (
                orig_tt.syntax().ancestors().find_map(ast::MacroCall::cast),
                spec_tt.syntax().ancestors().find_map(ast::MacroCall::cast),
            ) {
                let mac_call_path0 = actual_macro_call.path().as_ref().map(|s| s.syntax().text());
                let mac_call_path1 =
                    macro_call_with_fake_ident.path().as_ref().map(|s| s.syntax().text());

                // inconsistent state, stop expanding
                if mac_call_path0 != mac_call_path1 {
                    break 'expansion;
                }
                let speculative_args = match macro_call_with_fake_ident.token_tree() {
                    Some(tt) => tt,
                    None => break 'expansion,
                };

                match (
                    self.sema.expand(&actual_macro_call),
                    self.sema.speculative_expand(
                        &actual_macro_call,
                        &speculative_args,
                        fake_ident_token.clone(),
                    ),
                ) {
                    // successful expansions
                    (Some(actual_expansion), Some((fake_expansion, fake_mapped_token))) => {
                        let new_offset = fake_mapped_token.text_range().start();
                        if new_offset > actual_expansion.text_range().end() {
                            // offset outside of bounds from the original expansion,
                            // stop here to prevent problems from happening
                            break 'expansion;
                        }
                        original_file = actual_expansion;
                        speculative_file = fake_expansion;
                        fake_ident_token = fake_mapped_token;
                        offset = new_offset;
                        continue 'expansion;
                    }
                    // at least on expansion failed, we won't have anything to expand from this point
                    // onwards so break out
                    _ => break 'expansion,
                }
            }

            // none of our states have changed so stop the loop
            break 'expansion;
        }

        self.fill(&original_file, speculative_file, offset, derive_ctx)
    }

    /// Calculate the expected type and name of the cursor position.
    fn expected_type_and_name(&self) -> (Option<Type>, Option<NameOrNameRef>) {
        let mut node = match self.token.parent() {
            Some(it) => it,
            None => return (None, None),
        };
        loop {
            break match_ast! {
                match node {
                    ast::LetStmt(it) => {
                        cov_mark::hit!(expected_type_let_with_leading_char);
                        cov_mark::hit!(expected_type_let_without_leading_char);
                        let ty = it.pat()
                            .and_then(|pat| self.sema.type_of_pat(&pat))
                            .or_else(|| it.initializer().and_then(|it| self.sema.type_of_expr(&it)))
                            .map(TypeInfo::original);
                        let name = match it.pat() {
                            Some(ast::Pat::IdentPat(ident)) => ident.name().map(NameOrNameRef::Name),
                            Some(_) | None => None,
                        };

                        (ty, name)
                    },
                    ast::LetExpr(it) => {
                        cov_mark::hit!(expected_type_if_let_without_leading_char);
                        let ty = it.pat()
                            .and_then(|pat| self.sema.type_of_pat(&pat))
                            .or_else(|| it.expr().and_then(|it| self.sema.type_of_expr(&it)))
                            .map(TypeInfo::original);
                        (ty, None)
                    },
                    ast::ArgList(_) => {
                        cov_mark::hit!(expected_type_fn_param);
                        ActiveParameter::at_token(
                            &self.sema,
                            self.token.clone(),
                        ).map(|ap| {
                            let name = ap.ident().map(NameOrNameRef::Name);
                            let ty = if has_ref(&self.token) {
                                cov_mark::hit!(expected_type_fn_param_ref);
                                ap.ty.remove_ref()
                            } else {
                                Some(ap.ty)
                            };
                            (ty, name)
                        })
                        .unwrap_or((None, None))
                    },
                    ast::RecordExprFieldList(it) => {
                        // wouldn't try {} be nice...
                        (|| {
                            if self.token.kind() == T![..]
                                || self.token.prev_token().map(|t| t.kind()) == Some(T![..])
                            {
                                cov_mark::hit!(expected_type_struct_func_update);
                                let record_expr = it.syntax().parent().and_then(ast::RecordExpr::cast)?;
                                let ty = self.sema.type_of_expr(&record_expr.into())?;
                                Some((
                                    Some(ty.original),
                                    None
                                ))
                            } else {
                                cov_mark::hit!(expected_type_struct_field_without_leading_char);
                                let expr_field = self.token.prev_sibling_or_token()?
                                    .into_node()
                                    .and_then(ast::RecordExprField::cast)?;
                                let (_, _, ty) = self.sema.resolve_record_field(&expr_field)?;
                                Some((
                                    Some(ty),
                                    expr_field.field_name().map(NameOrNameRef::NameRef),
                                ))
                            }
                        })().unwrap_or((None, None))
                    },
                    ast::RecordExprField(it) => {
                        if let Some(expr) = it.expr() {
                            cov_mark::hit!(expected_type_struct_field_with_leading_char);
                            (
                                self.sema.type_of_expr(&expr).map(TypeInfo::original),
                                it.field_name().map(NameOrNameRef::NameRef),
                            )
                        } else {
                            cov_mark::hit!(expected_type_struct_field_followed_by_comma);
                            let ty = self.sema.resolve_record_field(&it)
                                .map(|(_, _, ty)| ty);
                            (
                                ty,
                                it.field_name().map(NameOrNameRef::NameRef),
                            )
                        }
                    },
                    ast::MatchExpr(it) => {
                        cov_mark::hit!(expected_type_match_arm_without_leading_char);
                        let ty = it.expr().and_then(|e| self.sema.type_of_expr(&e)).map(TypeInfo::original);
                        (ty, None)
                    },
                    ast::IfExpr(it) => {
                        let ty = it.condition()
                            .and_then(|e| self.sema.type_of_expr(&e))
                            .map(TypeInfo::original);
                        (ty, None)
                    },
                    ast::IdentPat(it) => {
                        cov_mark::hit!(expected_type_if_let_with_leading_char);
                        cov_mark::hit!(expected_type_match_arm_with_leading_char);
                        let ty = self.sema.type_of_pat(&ast::Pat::from(it)).map(TypeInfo::original);
                        (ty, None)
                    },
                    ast::Fn(it) => {
                        cov_mark::hit!(expected_type_fn_ret_with_leading_char);
                        cov_mark::hit!(expected_type_fn_ret_without_leading_char);
                        let def = self.sema.to_def(&it);
                        (def.map(|def| def.ret_type(self.db)), None)
                    },
                    ast::ClosureExpr(it) => {
                        let ty = self.sema.type_of_expr(&it.into());
                        ty.and_then(|ty| ty.original.as_callable(self.db))
                            .map(|c| (Some(c.return_type()), None))
                            .unwrap_or((None, None))
                    },
                    ast::ParamList(_) => (None, None),
                    ast::Stmt(_) => (None, None),
                    ast::Item(_) => (None, None),
                    _ => {
                        match node.parent() {
                            Some(n) => {
                                node = n;
                                continue;
                            },
                            None => (None, None),
                        }
                    },
                }
            };
        }
    }

    /// Fill the completion context, this is what does semantic reasoning about the surrounding context
    /// of the completion location.
    fn fill(
        &mut self,
        original_file: &SyntaxNode,
        file_with_fake_ident: SyntaxNode,
        offset: TextSize,
        derive_ctx: Option<(SyntaxNode, SyntaxNode, TextSize, ast::Attr)>,
    ) -> Option<()> {
        let fake_ident_token = file_with_fake_ident.token_at_offset(offset).right_biased().unwrap();
        let syntax_element = NodeOrToken::Token(fake_ident_token);
        if is_in_token_of_for_loop(syntax_element.clone()) {
            // for pat $0
            // there is nothing to complete here except `in` keyword
            // don't bother populating the context
            // FIXME: the completion calculations should end up good enough
            // such that this special case becomes unnecessary
            return None;
        }

        self.previous_token = previous_token(syntax_element.clone());

        self.incomplete_let =
            syntax_element.ancestors().take(6).find_map(ast::LetStmt::cast).map_or(false, |it| {
                it.syntax().text_range().end() == syntax_element.text_range().end()
            });

        (self.expected_type, self.expected_name) = self.expected_type_and_name();

        // Overwrite the path kind for derives
        if let Some((original_file, file_with_fake_ident, offset, origin_attr)) = derive_ctx {
            self.existing_derives = self
                .sema
                .resolve_derive_macro(&origin_attr)
                .into_iter()
                .flatten()
                .flatten()
                .collect();

            if let Some(ast::NameLike::NameRef(name_ref)) =
                find_node_at_offset(&file_with_fake_ident, offset)
            {
                let parent = name_ref.syntax().parent()?;
                let (mut nameref_ctx, _) =
                    Self::classify_name_ref(&self.sema, &original_file, name_ref, parent);
                if let Some(path_ctx) = &mut nameref_ctx.path_ctx {
                    path_ctx.kind = PathKind::Derive;
                }
                self.ident_ctx = IdentContext::NameRef(nameref_ctx);
                return Some(());
            }
            return None;
        }

        let name_like = match find_node_at_offset(&file_with_fake_ident, offset) {
            Some(it) => it,
            None => {
                if let Some(original) = ast::String::cast(self.original_token.clone()) {
                    self.ident_ctx = IdentContext::String {
                        original,
                        expanded: ast::String::cast(self.token.clone()),
                    };
                } else {
                    // Fix up trailing whitespace problem
                    // #[attr(foo = $0
                    let token = if self.token.kind() == SyntaxKind::WHITESPACE {
                        self.previous_token.as_ref()?
                    } else {
                        &self.token
                    };
                    let p = token.parent()?;
                    if p.kind() == SyntaxKind::TOKEN_TREE
                        && p.ancestors().any(|it| it.kind() == SyntaxKind::META)
                    {
                        self.ident_ctx = IdentContext::UnexpandedAttrTT {
                            fake_attribute_under_caret: syntax_element
                                .ancestors()
                                .find_map(ast::Attr::cast),
                        };
                    } else {
                        return None;
                    }
                }
                return Some(());
            }
        };
        self.completion_location =
            determine_location(&self.sema, original_file, offset, &name_like);
        self.prev_sibling = determine_prev_sibling(&name_like);
        self.impl_def = self
            .sema
            .token_ancestors_with_macros(self.token.clone())
            .take_while(|it| it.kind() != SOURCE_FILE && it.kind() != MODULE)
            .find_map(ast::Impl::cast);
        self.function_def = self
            .sema
            .token_ancestors_with_macros(self.token.clone())
            .take_while(|it| it.kind() != SOURCE_FILE && it.kind() != MODULE)
            .find_map(ast::Fn::cast);

        match name_like {
            ast::NameLike::Lifetime(lifetime) => {
                self.ident_ctx = IdentContext::Lifetime(Self::classify_lifetime(
                    &self.sema,
                    original_file,
                    lifetime,
                )?);
            }
            ast::NameLike::NameRef(name_ref) => {
                let parent = name_ref.syntax().parent()?;
                let (nameref_ctx, pat_ctx) =
                    Self::classify_name_ref(&self.sema, &original_file, name_ref, parent);
                self.ident_ctx = IdentContext::NameRef(nameref_ctx);
                self.pattern_ctx = pat_ctx;
            }
            ast::NameLike::Name(name) => {
                let (name_ctx, pat_ctx) = Self::classify_name(&self.sema, original_file, name)?;
                self.pattern_ctx = pat_ctx;
                self.ident_ctx = IdentContext::Name(name_ctx);
            }
        }
        Some(())
    }

    fn classify_lifetime(
        _sema: &Semantics<RootDatabase>,
        original_file: &SyntaxNode,
        lifetime: ast::Lifetime,
    ) -> Option<LifetimeContext> {
        let parent = lifetime.syntax().parent()?;
        if parent.kind() == ERROR {
            return None;
        }

        let kind = match_ast! {
            match parent {
                ast::LifetimeParam(param) => LifetimeKind::LifetimeParam {
                    is_decl: param.lifetime().as_ref() == Some(&lifetime),
                    param
                },
                ast::BreakExpr(_) => LifetimeKind::LabelRef,
                ast::ContinueExpr(_) => LifetimeKind::LabelRef,
                ast::Label(_) => LifetimeKind::LabelDef,
                _ => LifetimeKind::Lifetime,
            }
        };
        let lifetime = find_node_at_offset(&original_file, lifetime.syntax().text_range().start());

        Some(LifetimeContext { lifetime, kind })
    }

    fn classify_name(
        _sema: &Semantics<RootDatabase>,
        original_file: &SyntaxNode,
        name: ast::Name,
    ) -> Option<(NameContext, Option<PatternContext>)> {
        let parent = name.syntax().parent()?;
        let mut pat_ctx = None;
        let kind = match_ast! {
            match parent {
                ast::Const(_) => NameKind::Const,
                ast::ConstParam(_) => NameKind::ConstParam,
                ast::Enum(_) => NameKind::Enum,
                ast::Fn(_) => NameKind::Function,
                ast::IdentPat(bind_pat) => {
                    let is_name_in_field_pat = bind_pat
                        .syntax()
                        .parent()
                        .and_then(ast::RecordPatField::cast)
                        .map_or(false, |pat_field| pat_field.name_ref().is_none());
                    if !is_name_in_field_pat {
                        pat_ctx = Some(pattern_context_for(original_file, bind_pat.into()));
                    }

                    NameKind::IdentPat
                },
                ast::MacroDef(_) => NameKind::MacroDef,
                ast::MacroRules(_) => NameKind::MacroRules,
                ast::Module(module) => NameKind::Module(module),
                ast::RecordField(_) => NameKind::RecordField,
                ast::Rename(_) => NameKind::Rename,
                ast::SelfParam(_) => NameKind::SelfParam,
                ast::Static(_) => NameKind::Static,
                ast::Struct(_) => NameKind::Struct,
                ast::Trait(_) => NameKind::Trait,
                ast::TypeAlias(_) => NameKind::TypeAlias,
                ast::TypeParam(_) => NameKind::TypeParam,
                ast::Union(_) => NameKind::Union,
                ast::Variant(_) => NameKind::Variant,
                _ => return None,
            }
        };
        let name = find_node_at_offset(&original_file, name.syntax().text_range().start());
        Some((NameContext { name, kind }, pat_ctx))
    }

    fn classify_name_ref(
        sema: &Semantics<RootDatabase>,
        original_file: &SyntaxNode,
        name_ref: ast::NameRef,
        parent: SyntaxNode,
    ) -> (NameRefContext, Option<PatternContext>) {
        let nameref = find_node_at_offset(&original_file, name_ref.syntax().text_range().start());

        let mut nameref_ctx = NameRefContext { dot_access: None, path_ctx: None, nameref };

        let segment = match_ast! {
            match parent {
                ast::PathSegment(segment) => segment,
                ast::FieldExpr(field) => {
                    let receiver = find_in_original_file(field.expr(), original_file);
                    let receiver_is_ambiguous_float_literal = match &receiver {
                        Some(ast::Expr::Literal(l)) => matches! {
                            l.kind(),
                            ast::LiteralKind::FloatNumber { .. } if l.syntax().last_token().map_or(false, |it| it.text().ends_with('.'))
                        },
                        _ => false,
                    };
                    nameref_ctx.dot_access = Some(DotAccess::Field { receiver, receiver_is_ambiguous_float_literal });
                    return (nameref_ctx, None);
                },
                ast::MethodCallExpr(method) => {
                    nameref_ctx.dot_access = Some(
                        DotAccess::Method {
                            receiver: find_in_original_file(method.receiver(), original_file),
                            has_parens: method.arg_list().map_or(false, |it| it.l_paren_token().is_some())
                        }
                    );
                    return (nameref_ctx, None);
                },
                _ => return (nameref_ctx, None),
            }
        };

        let path = segment.parent_path();
        let mut path_ctx = PathCompletionCtx {
            has_call_parens: false,
            has_macro_bang: false,
            is_absolute_path: false,
            qualifier: None,
            parent: path.parent_path(),
            kind: PathKind::Item { kind: ItemListKind::SourceFile },
            has_type_args: false,
        };
        let mut pat_ctx = None;

        let is_in_block = |it: &SyntaxNode| {
            it.parent()
                .map(|node| {
                    ast::ExprStmt::can_cast(node.kind()) || ast::StmtList::can_cast(node.kind())
                })
                .unwrap_or(false)
        };
        let is_in_func_update = |it: &SyntaxNode| {
            it.parent().map_or(false, |it| ast::RecordExprFieldList::can_cast(it.kind()))
        };

        let kind = path.syntax().ancestors().find_map(|it| {
            // using Option<Option<PathKind>> as extra controlflow
            let kind = match_ast! {
                match it {
                    ast::PathType(_) => Some(PathKind::Type),
                    ast::PathExpr(it) => {
                        path_ctx.has_call_parens = it.syntax().parent().map_or(false, |it| ast::CallExpr::can_cast(it.kind()));
                        let in_block_expr = is_in_block(it.syntax());
                        let in_loop_body = is_in_loop_body(it.syntax());
                        let in_functional_update = is_in_func_update(it.syntax());
                        Some(PathKind::Expr { in_block_expr, in_loop_body, in_functional_update })
                    },
                    ast::TupleStructPat(it) => {
                        path_ctx.has_call_parens = true;
                        pat_ctx = Some(pattern_context_for(original_file, it.into()));
                        Some(PathKind::Pat)
                    },
                    ast::RecordPat(it) => {
                        path_ctx.has_call_parens = true;
                        pat_ctx = Some(pattern_context_for(original_file, it.into()));
                        Some(PathKind::Pat)
                    },
                    ast::PathPat(it) => {
                        pat_ctx = Some(pattern_context_for(original_file, it.into()));
                        Some(PathKind::Pat)
                    },
                    ast::MacroCall(it) => {
                        path_ctx.has_macro_bang = it.excl_token().is_some();
                        let parent = it.syntax().parent();
                        match parent.as_ref().map(|it| it.kind()) {
                            Some(SyntaxKind::MACRO_PAT) => Some(PathKind::Pat),
                            Some(SyntaxKind::MACRO_TYPE) => Some(PathKind::Type),
                            Some(SyntaxKind::ITEM_LIST) => Some(PathKind::Item { kind: ItemListKind::Module }),
                            Some(SyntaxKind::ASSOC_ITEM_LIST) => Some(PathKind::Item { kind: match parent.and_then(|it| it.parent()).map(|it| it.kind()) {
                                Some(SyntaxKind::TRAIT) => ItemListKind::Trait,
                                Some(SyntaxKind::IMPL) => ItemListKind::Impl,
                                _ => return Some(None),
                            } }),
                            Some(SyntaxKind::EXTERN_ITEM_LIST) => Some(PathKind::Item { kind: ItemListKind::ExternBlock }),
                            Some(SyntaxKind::SOURCE_FILE) => Some(PathKind::Item { kind: ItemListKind::SourceFile }),
                            _ => {
                               return Some(parent.and_then(ast::MacroExpr::cast).map(|it| {
                                    let in_loop_body = is_in_loop_body(it.syntax());
                                    let in_block_expr = is_in_block(it.syntax());
                                    let in_functional_update = is_in_func_update(it.syntax());
                                    PathKind::Expr { in_block_expr, in_loop_body, in_functional_update }
                                }));
                            },
                        }
                    },
                    ast::Meta(meta) => (|| {
                        let attr = meta.parent_attr()?;
                        let kind = attr.kind();
                        let attached = attr.syntax().parent()?;
                        let is_trailing_outer_attr = kind != AttrKind::Inner
                            && non_trivia_sibling(attr.syntax().clone().into(), syntax::Direction::Next).is_none();
                        let annotated_item_kind = if is_trailing_outer_attr {
                            None
                        } else {
                            Some(attached.kind())
                        };
                        Some(PathKind::Attr {
                            kind,
                            annotated_item_kind,
                        })
                    })(),
                    ast::Visibility(it) => Some(PathKind::Vis { has_in_token: it.in_token().is_some() }),
                    ast::UseTree(_) => Some(PathKind::Use),
                    ast::ItemList(_) => Some(PathKind::Item { kind: ItemListKind::Module }),
                    ast::AssocItemList(it) => Some(PathKind::Item { kind: {
                            match it.syntax().parent()?.kind() {
                                SyntaxKind::TRAIT => ItemListKind::Trait,
                                SyntaxKind::IMPL => ItemListKind::Impl,
                                _ => return None,
                            }
                        }}),
                    ast::ExternItemList(_) => Some(PathKind::Item { kind: ItemListKind::ExternBlock }),
                    ast::SourceFile(_) => Some(PathKind::Item { kind: ItemListKind::SourceFile }),
                    _ => return None,
                }
            };
            Some(kind)
        }).flatten();
        match kind {
            Some(kind) => path_ctx.kind = kind,
            None => return (nameref_ctx, pat_ctx),
        }
        path_ctx.has_type_args = segment.generic_arg_list().is_some();

        if let Some((path, use_tree_parent)) = path_or_use_tree_qualifier(&path) {
            if !use_tree_parent {
                path_ctx.is_absolute_path =
                    path.top_path().segment().map_or(false, |it| it.coloncolon_token().is_some());
            }

            let path = path
                .segment()
                .and_then(|it| find_node_in_file(original_file, &it))
                .map(|it| it.parent_path());
            path_ctx.qualifier = path.map(|path| {
                let res = sema.resolve_path(&path);
                let is_super_chain = iter::successors(Some(path.clone()), |p| p.qualifier())
                    .all(|p| p.segment().and_then(|s| s.super_token()).is_some());

                // `<_>::$0`
                let is_infer_qualifier = path.qualifier().is_none()
                    && matches!(
                        path.segment().and_then(|it| it.kind()),
                        Some(ast::PathSegmentKind::Type {
                            type_ref: Some(ast::Type::InferType(_)),
                            trait_ref: None,
                        })
                    );

                PathQualifierCtx {
                    path,
                    resolution: res,
                    is_super_chain,
                    use_tree_parent,
                    is_infer_qualifier,
                }
            });
        } else if let Some(segment) = path.segment() {
            if segment.coloncolon_token().is_some() {
                path_ctx.is_absolute_path = true;
            }
        }
        nameref_ctx.path_ctx = Some(path_ctx);
        (nameref_ctx, pat_ctx)
    }
}

fn pattern_context_for(original_file: &SyntaxNode, pat: ast::Pat) -> PatternContext {
    let mut is_param = None;
    let (refutability, has_type_ascription) =
    pat
        .syntax()
        .ancestors()
        .skip_while(|it| ast::Pat::can_cast(it.kind()))
        .next()
        .map_or((PatternRefutability::Irrefutable, false), |node| {
            let refutability = match_ast! {
                match node {
                    ast::LetStmt(let_) => return (PatternRefutability::Irrefutable, let_.ty().is_some()),
                    ast::Param(param) => {
                        let has_type_ascription = param.ty().is_some();
                        is_param = (|| {
                            let fake_param_list = param.syntax().parent().and_then(ast::ParamList::cast)?;
                            let param_list = find_node_in_file_compensated(original_file, &fake_param_list)?;
                            let param_list_owner = param_list.syntax().parent()?;
                            let kind = match_ast! {
                                match param_list_owner {
                                    ast::ClosureExpr(closure) => ParamKind::Closure(closure),
                                    ast::Fn(fn_) => ParamKind::Function(fn_),
                                    _ => return None,
                                }
                            };
                            Some((param_list, param, kind))
                        })();
                        return (PatternRefutability::Irrefutable, has_type_ascription)
                    },
                    ast::MatchArm(_) => PatternRefutability::Refutable,
                    ast::LetExpr(_) => PatternRefutability::Refutable,
                    ast::ForExpr(_) => PatternRefutability::Irrefutable,
                    _ => PatternRefutability::Irrefutable,
                }
            };
            (refutability, false)
        });
    let (ref_token, mut_token) = match &pat {
        ast::Pat::IdentPat(it) => (it.ref_token(), it.mut_token()),
        _ => (None, None),
    };
    PatternContext {
        refutability,
        param_ctx: is_param,
        has_type_ascription,
        parent_pat: pat.syntax().parent().and_then(ast::Pat::cast),
        mut_token,
        ref_token,
    }
}

fn find_in_original_file<N: AstNode>(x: Option<N>, original_file: &SyntaxNode) -> Option<N> {
    fn find_node_with_range<N: AstNode>(syntax: &SyntaxNode, range: TextRange) -> Option<N> {
        let range = syntax.text_range().intersect(range)?;
        syntax.covering_element(range).ancestors().find_map(N::cast)
    }
    x.map(|e| e.syntax().text_range()).and_then(|r| find_node_with_range(original_file, r))
}

/// Attempts to find `node` inside `syntax` via `node`'s text range.
fn find_node_in_file<N: AstNode>(syntax: &SyntaxNode, node: &N) -> Option<N> {
    let syntax_range = syntax.text_range();
    let range = node.syntax().text_range();
    let intersection = range.intersect(syntax_range)?;
    syntax.covering_element(intersection).ancestors().find_map(N::cast)
}

/// Attempts to find `node` inside `syntax` via `node`'s text range while compensating
/// for the offset introduced by the fake ident.
/// This is wrong if `node` comes before the insertion point! Use `find_node_in_file` instead.
fn find_node_in_file_compensated<N: AstNode>(syntax: &SyntaxNode, node: &N) -> Option<N> {
    let syntax_range = syntax.text_range();
    let range = node.syntax().text_range();
    let end = range.end().checked_sub(TextSize::try_from(COMPLETION_MARKER.len()).ok()?)?;
    if end < range.start() {
        return None;
    }
    let range = TextRange::new(range.start(), end);
    // our inserted ident could cause `range` to be go outside of the original syntax, so cap it
    let intersection = range.intersect(syntax_range)?;
    syntax.covering_element(intersection).ancestors().find_map(N::cast)
}

fn path_or_use_tree_qualifier(path: &ast::Path) -> Option<(ast::Path, bool)> {
    if let Some(qual) = path.qualifier() {
        return Some((qual, false));
    }
    let use_tree_list = path.syntax().ancestors().find_map(ast::UseTreeList::cast)?;
    let use_tree = use_tree_list.syntax().parent().and_then(ast::UseTree::cast)?;
    Some((use_tree.path()?, true))
}

fn has_ref(token: &SyntaxToken) -> bool {
    let mut token = token.clone();
    for skip in [IDENT, WHITESPACE, T![mut]] {
        if token.kind() == skip {
            token = match token.prev_token() {
                Some(it) => it,
                None => return false,
            }
        }
    }
    token.kind() == T![&]
}

const OP_TRAIT_LANG_NAMES: &[&str] = &[
    "add_assign",
    "add",
    "bitand_assign",
    "bitand",
    "bitor_assign",
    "bitor",
    "bitxor_assign",
    "bitxor",
    "deref_mut",
    "deref",
    "div_assign",
    "div",
    "eq",
    "fn_mut",
    "fn_once",
    "fn",
    "index_mut",
    "index",
    "mul_assign",
    "mul",
    "neg",
    "not",
    "partial_ord",
    "rem_assign",
    "rem",
    "shl_assign",
    "shl",
    "shr_assign",
    "shr",
    "sub",
];

#[cfg(test)]
mod tests {
    use expect_test::{expect, Expect};
    use hir::HirDisplay;

    use crate::tests::{position, TEST_CONFIG};

    use super::CompletionContext;

    fn check_expected_type_and_name(ra_fixture: &str, expect: Expect) {
        let (db, pos) = position(ra_fixture);
        let config = TEST_CONFIG;
        let completion_context = CompletionContext::new(&db, pos, &config).unwrap();

        let ty = completion_context
            .expected_type
            .map(|t| t.display_test(&db).to_string())
            .unwrap_or("?".to_owned());

        let name = completion_context
            .expected_name
            .map_or_else(|| "?".to_owned(), |name| name.to_string());

        expect.assert_eq(&format!("ty: {}, name: {}", ty, name));
    }

    #[test]
    fn expected_type_let_without_leading_char() {
        cov_mark::check!(expected_type_let_without_leading_char);
        check_expected_type_and_name(
            r#"
fn foo() {
    let x: u32 = $0;
}
"#,
            expect![[r#"ty: u32, name: x"#]],
        );
    }

    #[test]
    fn expected_type_let_with_leading_char() {
        cov_mark::check!(expected_type_let_with_leading_char);
        check_expected_type_and_name(
            r#"
fn foo() {
    let x: u32 = c$0;
}
"#,
            expect![[r#"ty: u32, name: x"#]],
        );
    }

    #[test]
    fn expected_type_let_pat() {
        check_expected_type_and_name(
            r#"
fn foo() {
    let x$0 = 0u32;
}
"#,
            expect![[r#"ty: u32, name: ?"#]],
        );
        check_expected_type_and_name(
            r#"
fn foo() {
    let $0 = 0u32;
}
"#,
            expect![[r#"ty: u32, name: ?"#]],
        );
    }

    #[test]
    fn expected_type_fn_param() {
        cov_mark::check!(expected_type_fn_param);
        check_expected_type_and_name(
            r#"
fn foo() { bar($0); }
fn bar(x: u32) {}
"#,
            expect![[r#"ty: u32, name: x"#]],
        );
        check_expected_type_and_name(
            r#"
fn foo() { bar(c$0); }
fn bar(x: u32) {}
"#,
            expect![[r#"ty: u32, name: x"#]],
        );
    }

    #[test]
    fn expected_type_fn_param_ref() {
        cov_mark::check!(expected_type_fn_param_ref);
        check_expected_type_and_name(
            r#"
fn foo() { bar(&$0); }
fn bar(x: &u32) {}
"#,
            expect![[r#"ty: u32, name: x"#]],
        );
        check_expected_type_and_name(
            r#"
fn foo() { bar(&mut $0); }
fn bar(x: &mut u32) {}
"#,
            expect![[r#"ty: u32, name: x"#]],
        );
        check_expected_type_and_name(
            r#"
fn foo() { bar(& c$0); }
fn bar(x: &u32) {}
        "#,
            expect![[r#"ty: u32, name: x"#]],
        );
        check_expected_type_and_name(
            r#"
fn foo() { bar(&mut c$0); }
fn bar(x: &mut u32) {}
"#,
            expect![[r#"ty: u32, name: x"#]],
        );
        check_expected_type_and_name(
            r#"
fn foo() { bar(&c$0); }
fn bar(x: &u32) {}
        "#,
            expect![[r#"ty: u32, name: x"#]],
        );
    }

    #[test]
    fn expected_type_struct_field_without_leading_char() {
        cov_mark::check!(expected_type_struct_field_without_leading_char);
        check_expected_type_and_name(
            r#"
struct Foo { a: u32 }
fn foo() {
    Foo { a: $0 };
}
"#,
            expect![[r#"ty: u32, name: a"#]],
        )
    }

    #[test]
    fn expected_type_struct_field_followed_by_comma() {
        cov_mark::check!(expected_type_struct_field_followed_by_comma);
        check_expected_type_and_name(
            r#"
struct Foo { a: u32 }
fn foo() {
    Foo { a: $0, };
}
"#,
            expect![[r#"ty: u32, name: a"#]],
        )
    }

    #[test]
    fn expected_type_generic_struct_field() {
        check_expected_type_and_name(
            r#"
struct Foo<T> { a: T }
fn foo() -> Foo<u32> {
    Foo { a: $0 }
}
"#,
            expect![[r#"ty: u32, name: a"#]],
        )
    }

    #[test]
    fn expected_type_struct_field_with_leading_char() {
        cov_mark::check!(expected_type_struct_field_with_leading_char);
        check_expected_type_and_name(
            r#"
struct Foo { a: u32 }
fn foo() {
    Foo { a: c$0 };
}
"#,
            expect![[r#"ty: u32, name: a"#]],
        );
    }

    #[test]
    fn expected_type_match_arm_without_leading_char() {
        cov_mark::check!(expected_type_match_arm_without_leading_char);
        check_expected_type_and_name(
            r#"
enum E { X }
fn foo() {
   match E::X { $0 }
}
"#,
            expect![[r#"ty: E, name: ?"#]],
        );
    }

    #[test]
    fn expected_type_match_arm_with_leading_char() {
        cov_mark::check!(expected_type_match_arm_with_leading_char);
        check_expected_type_and_name(
            r#"
enum E { X }
fn foo() {
   match E::X { c$0 }
}
"#,
            expect![[r#"ty: E, name: ?"#]],
        );
    }

    #[test]
    fn expected_type_if_let_without_leading_char() {
        cov_mark::check!(expected_type_if_let_without_leading_char);
        check_expected_type_and_name(
            r#"
enum Foo { Bar, Baz, Quux }

fn foo() {
    let f = Foo::Quux;
    if let $0 = f { }
}
"#,
            expect![[r#"ty: Foo, name: ?"#]],
        )
    }

    #[test]
    fn expected_type_if_let_with_leading_char() {
        cov_mark::check!(expected_type_if_let_with_leading_char);
        check_expected_type_and_name(
            r#"
enum Foo { Bar, Baz, Quux }

fn foo() {
    let f = Foo::Quux;
    if let c$0 = f { }
}
"#,
            expect![[r#"ty: Foo, name: ?"#]],
        )
    }

    #[test]
    fn expected_type_fn_ret_without_leading_char() {
        cov_mark::check!(expected_type_fn_ret_without_leading_char);
        check_expected_type_and_name(
            r#"
fn foo() -> u32 {
    $0
}
"#,
            expect![[r#"ty: u32, name: ?"#]],
        )
    }

    #[test]
    fn expected_type_fn_ret_with_leading_char() {
        cov_mark::check!(expected_type_fn_ret_with_leading_char);
        check_expected_type_and_name(
            r#"
fn foo() -> u32 {
    c$0
}
"#,
            expect![[r#"ty: u32, name: ?"#]],
        )
    }

    #[test]
    fn expected_type_fn_ret_fn_ref_fully_typed() {
        check_expected_type_and_name(
            r#"
fn foo() -> u32 {
    foo$0
}
"#,
            expect![[r#"ty: u32, name: ?"#]],
        )
    }

    #[test]
    fn expected_type_closure_param_return() {
        // FIXME: make this work with `|| $0`
        check_expected_type_and_name(
            r#"
//- minicore: fn
fn foo() {
    bar(|| a$0);
}

fn bar(f: impl FnOnce() -> u32) {}
"#,
            expect![[r#"ty: u32, name: ?"#]],
        );
    }

    #[test]
    fn expected_type_generic_function() {
        check_expected_type_and_name(
            r#"
fn foo() {
    bar::<u32>($0);
}

fn bar<T>(t: T) {}
"#,
            expect![[r#"ty: u32, name: t"#]],
        );
    }

    #[test]
    fn expected_type_generic_method() {
        check_expected_type_and_name(
            r#"
fn foo() {
    S(1u32).bar($0);
}

struct S<T>(T);
impl<T> S<T> {
    fn bar(self, t: T) {}
}
"#,
            expect![[r#"ty: u32, name: t"#]],
        );
    }

    #[test]
    fn expected_type_functional_update() {
        cov_mark::check!(expected_type_struct_func_update);
        check_expected_type_and_name(
            r#"
struct Foo { field: u32 }
fn foo() {
    Foo {
        ..$0
    }
}
"#,
            expect![[r#"ty: Foo, name: ?"#]],
        );
    }

    #[test]
    fn expected_type_param_pat() {
        check_expected_type_and_name(
            r#"
struct Foo { field: u32 }
fn foo(a$0: Foo) {}
"#,
            expect![[r#"ty: Foo, name: ?"#]],
        );
        check_expected_type_and_name(
            r#"
struct Foo { field: u32 }
fn foo($0: Foo) {}
"#,
            // FIXME make this work, currently fails due to pattern recovery eating the `:`
            expect![[r#"ty: ?, name: ?"#]],
        );
    }
}
