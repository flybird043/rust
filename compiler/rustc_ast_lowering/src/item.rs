use super::{AnonymousLifetimeMode, LoweringContext, ParamMode};
use super::{ImplTraitContext, ImplTraitPosition};
use crate::Arena;

use rustc_ast::node_id::NodeMap;
use rustc_ast::ptr::P;
use rustc_ast::visit::{self, AssocCtxt, FnCtxt, FnKind, Visitor};
use rustc_ast::*;
use rustc_data_structures::fx::FxHashSet;
use rustc_errors::struct_span_err;
use rustc_hir as hir;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::def_id::LocalDefId;
use rustc_span::source_map::{respan, DesugaringKind};
use rustc_span::symbol::{kw, sym, Ident};
use rustc_span::Span;
use rustc_target::spec::abi;
use smallvec::{smallvec, SmallVec};
use tracing::debug;

use std::mem;

pub(super) struct ItemLowerer<'a, 'lowering, 'hir> {
    pub(super) lctx: &'a mut LoweringContext<'lowering, 'hir>,
}

impl ItemLowerer<'_, '_, '_> {
    fn with_trait_impl_ref(&mut self, impl_ref: &Option<TraitRef>, f: impl FnOnce(&mut Self)) {
        let old = self.lctx.is_in_trait_impl;
        self.lctx.is_in_trait_impl = impl_ref.is_some();
        f(self);
        self.lctx.is_in_trait_impl = old;
    }
}

impl<'a> Visitor<'a> for ItemLowerer<'a, '_, '_> {
    fn visit_item(&mut self, item: &'a Item) {
        let mut item_hir_id = None;
        self.lctx.with_hir_id_owner(item.id, |lctx| {
            lctx.without_in_scope_lifetime_defs(|lctx| {
                if let Some(hir_item) = lctx.lower_item(item) {
                    let id = lctx.insert_item(hir_item);
                    item_hir_id = Some(id);
                }
            })
        });

        if let Some(hir_id) = item_hir_id {
            self.lctx.with_parent_item_lifetime_defs(hir_id, |this| {
                let this = &mut ItemLowerer { lctx: this };
                match item.kind {
                    ItemKind::Mod(..) => {
                        let def_id = this.lctx.lower_node_id(item.id).expect_owner();
                        let old_current_module =
                            mem::replace(&mut this.lctx.current_module, def_id);
                        visit::walk_item(this, item);
                        this.lctx.current_module = old_current_module;
                    }
                    ItemKind::Impl(box ImplKind { ref of_trait, .. }) => {
                        this.with_trait_impl_ref(of_trait, |this| visit::walk_item(this, item));
                    }
                    _ => visit::walk_item(this, item),
                }
            });
        }
    }

    fn visit_fn(&mut self, fk: FnKind<'a>, sp: Span, _: NodeId) {
        match fk {
            FnKind::Fn(FnCtxt::Foreign, _, sig, _, _) => {
                self.visit_fn_header(&sig.header);
                visit::walk_fn_decl(self, &sig.decl);
                // Don't visit the foreign function body even if it has one, since lowering the
                // body would have no meaning and will have already been caught as a parse error.
            }
            _ => visit::walk_fn(self, fk, sp),
        }
    }

    fn visit_assoc_item(&mut self, item: &'a AssocItem, ctxt: AssocCtxt) {
        self.lctx.with_hir_id_owner(item.id, |lctx| match ctxt {
            AssocCtxt::Trait => {
                let hir_item = lctx.lower_trait_item(item);
                let id = hir_item.trait_item_id();
                lctx.trait_items.insert(id, hir_item);
                lctx.modules.entry(lctx.current_module).or_default().trait_items.insert(id);
            }
            AssocCtxt::Impl => {
                let hir_item = lctx.lower_impl_item(item);
                let id = hir_item.impl_item_id();
                lctx.impl_items.insert(id, hir_item);
                lctx.modules.entry(lctx.current_module).or_default().impl_items.insert(id);
            }
        });

        visit::walk_assoc_item(self, item, ctxt);
    }

    fn visit_foreign_item(&mut self, item: &'a ForeignItem) {
        self.lctx.allocate_hir_id_counter(item.id);
        self.lctx.with_hir_id_owner(item.id, |lctx| {
            let hir_item = lctx.lower_foreign_item(item);
            let id = hir_item.foreign_item_id();
            lctx.foreign_items.insert(id, hir_item);
            lctx.modules.entry(lctx.current_module).or_default().foreign_items.insert(id);
        });

        visit::walk_foreign_item(self, item);
    }
}

impl<'hir> LoweringContext<'_, 'hir> {
    // Same as the method above, but accepts `hir::GenericParam`s
    // instead of `ast::GenericParam`s.
    // This should only be used with generics that have already had their
    // in-band lifetimes added. In practice, this means that this function is
    // only used when lowering a child item of a trait or impl.
    fn with_parent_item_lifetime_defs<T>(
        &mut self,
        parent_hir_id: hir::ItemId,
        f: impl FnOnce(&mut LoweringContext<'_, '_>) -> T,
    ) -> T {
        let old_len = self.in_scope_lifetimes.len();

        let parent_generics = match self.items.get(&parent_hir_id).unwrap().kind {
            hir::ItemKind::Impl(hir::Impl { ref generics, .. })
            | hir::ItemKind::Trait(_, _, ref generics, ..) => generics.params,
            _ => &[],
        };
        let lt_def_names = parent_generics.iter().filter_map(|param| match param.kind {
            hir::GenericParamKind::Lifetime { .. } => Some(param.name.normalize_to_macros_2_0()),
            _ => None,
        });
        self.in_scope_lifetimes.extend(lt_def_names);

        let res = f(self);

        self.in_scope_lifetimes.truncate(old_len);
        res
    }

    // Clears (and restores) the `in_scope_lifetimes` field. Used when
    // visiting nested items, which never inherit in-scope lifetimes
    // from their surrounding environment.
    fn without_in_scope_lifetime_defs<T>(
        &mut self,
        f: impl FnOnce(&mut LoweringContext<'_, '_>) -> T,
    ) -> T {
        let old_in_scope_lifetimes = mem::replace(&mut self.in_scope_lifetimes, vec![]);

        // this vector is only used when walking over impl headers,
        // input types, and the like, and should not be non-empty in
        // between items
        assert!(self.lifetimes_to_define.is_empty());

        let res = f(self);

        assert!(self.in_scope_lifetimes.is_empty());
        self.in_scope_lifetimes = old_in_scope_lifetimes;

        res
    }

    pub(super) fn lower_mod(&mut self, items: &[P<Item>], inner: Span) -> hir::Mod<'hir> {
        hir::Mod {
            inner,
            item_ids: self.arena.alloc_from_iter(items.iter().flat_map(|x| self.lower_item_id(x))),
        }
    }

    pub(super) fn lower_item_id(&mut self, i: &Item) -> SmallVec<[hir::ItemId; 1]> {
        let node_ids = match i.kind {
            ItemKind::Use(ref use_tree) => {
                let mut vec = smallvec![i.id];
                self.lower_item_id_use_tree(use_tree, i.id, &mut vec);
                vec
            }
            ItemKind::MacroDef(..) => SmallVec::new(),
            ItemKind::Fn(..) | ItemKind::Impl(box ImplKind { of_trait: None, .. }) => {
                smallvec![i.id]
            }
            _ => smallvec![i.id],
        };

        node_ids
            .into_iter()
            .map(|node_id| hir::ItemId {
                def_id: self.allocate_hir_id_counter(node_id).expect_owner(),
            })
            .collect()
    }

    fn lower_item_id_use_tree(
        &mut self,
        tree: &UseTree,
        base_id: NodeId,
        vec: &mut SmallVec<[NodeId; 1]>,
    ) {
        match tree.kind {
            UseTreeKind::Nested(ref nested_vec) => {
                for &(ref nested, id) in nested_vec {
                    vec.push(id);
                    self.lower_item_id_use_tree(nested, id, vec);
                }
            }
            UseTreeKind::Glob => {}
            UseTreeKind::Simple(_, id1, id2) => {
                for (_, &id) in
                    self.expect_full_res_from_use(base_id).skip(1).zip([id1, id2].iter())
                {
                    vec.push(id);
                }
            }
        }
    }

    pub fn lower_item(&mut self, i: &Item) -> Option<hir::Item<'hir>> {
        let mut ident = i.ident;
        let mut vis = self.lower_visibility(&i.vis, None);

        if let ItemKind::MacroDef(MacroDef { ref body, macro_rules }) = i.kind {
            if !macro_rules || self.sess.contains_name(&i.attrs, sym::macro_export) {
                let hir_id = self.lower_node_id(i.id);
                self.lower_attrs(hir_id, &i.attrs);
                let body = P(self.lower_mac_args(body));
                self.exported_macros.push(hir::MacroDef {
                    ident,
                    vis,
                    def_id: hir_id.expect_owner(),
                    span: i.span,
                    ast: MacroDef { body, macro_rules },
                });
            } else {
                for a in i.attrs.iter() {
                    let a = self.lower_attr(a);
                    self.non_exported_macro_attrs.push(a);
                }
            }
            return None;
        }

        let hir_id = self.lower_node_id(i.id);
        let attrs = self.lower_attrs(hir_id, &i.attrs);
        let kind = self.lower_item_kind(i.span, i.id, hir_id, &mut ident, attrs, &mut vis, &i.kind);
        Some(hir::Item { def_id: hir_id.expect_owner(), ident, kind, vis, span: i.span })
    }

    fn lower_item_kind(
        &mut self,
        span: Span,
        id: NodeId,
        hir_id: hir::HirId,
        ident: &mut Ident,
        attrs: Option<&'hir [Attribute]>,
        vis: &mut hir::Visibility<'hir>,
        i: &ItemKind,
    ) -> hir::ItemKind<'hir> {
        match *i {
            ItemKind::ExternCrate(orig_name) => hir::ItemKind::ExternCrate(orig_name),
            ItemKind::Use(ref use_tree) => {
                // Start with an empty prefix.
                let prefix = Path { segments: vec![], span: use_tree.span, tokens: None };

                self.lower_use_tree(use_tree, &prefix, id, vis, ident, attrs)
            }
            ItemKind::Static(ref t, m, ref e) => {
                let (ty, body_id) = self.lower_const_item(t, span, e.as_deref());
                hir::ItemKind::Static(ty, m, body_id)
            }
            ItemKind::Const(_, ref t, ref e) => {
                let (ty, body_id) = self.lower_const_item(t, span, e.as_deref());
                hir::ItemKind::Const(ty, body_id)
            }
            ItemKind::Fn(box FnKind(
                _,
                FnSig { ref decl, header, span: fn_sig_span },
                ref generics,
                ref body,
            )) => {
                let fn_def_id = self.resolver.local_def_id(id);
                self.with_new_scopes(|this| {
                    this.current_item = Some(ident.span);

                    // Note: we don't need to change the return type from `T` to
                    // `impl Future<Output = T>` here because lower_body
                    // only cares about the input argument patterns in the function
                    // declaration (decl), not the return types.
                    let asyncness = header.asyncness;
                    let body_id =
                        this.lower_maybe_async_body(span, &decl, asyncness, body.as_deref());

                    let (generics, decl) = this.add_in_band_defs(
                        generics,
                        fn_def_id,
                        AnonymousLifetimeMode::PassThrough,
                        |this, idty| {
                            let ret_id = asyncness.opt_return_id();
                            this.lower_fn_decl(
                                &decl,
                                Some((fn_def_id.to_def_id(), idty)),
                                true,
                                ret_id,
                            )
                        },
                    );
                    let sig = hir::FnSig {
                        decl,
                        header: this.lower_fn_header(header, fn_sig_span, id),
                        span: fn_sig_span,
                    };
                    hir::ItemKind::Fn(sig, generics, body_id)
                })
            }
            ItemKind::Mod(_, ref mod_kind) => match mod_kind {
                ModKind::Loaded(items, _, inner_span) => {
                    hir::ItemKind::Mod(self.lower_mod(items, *inner_span))
                }
                ModKind::Unloaded => panic!("`mod` items should have been loaded by now"),
            },
            ItemKind::ForeignMod(ref fm) => {
                if fm.abi.is_none() {
                    self.maybe_lint_missing_abi(span, id, abi::Abi::C);
                }
                hir::ItemKind::ForeignMod {
                    abi: fm.abi.map_or(abi::Abi::C, |abi| self.lower_abi(abi)),
                    items: self
                        .arena
                        .alloc_from_iter(fm.items.iter().map(|x| self.lower_foreign_item_ref(x))),
                }
            }
            ItemKind::GlobalAsm(ref ga) => hir::ItemKind::GlobalAsm(self.lower_global_asm(ga)),
            ItemKind::TyAlias(box TyAliasKind(_, ref gen, _, Some(ref ty))) => {
                // We lower
                //
                // type Foo = impl Trait
                //
                // to
                //
                // type Foo = Foo1
                // opaque type Foo1: Trait
                let ty = self.lower_ty(
                    ty,
                    ImplTraitContext::OtherOpaqueTy {
                        capturable_lifetimes: &mut FxHashSet::default(),
                        origin: hir::OpaqueTyOrigin::Misc,
                    },
                );
                let generics = self.lower_generics(gen, ImplTraitContext::disallowed());
                hir::ItemKind::TyAlias(ty, generics)
            }
            ItemKind::TyAlias(box TyAliasKind(_, ref generics, _, None)) => {
                let ty = self.arena.alloc(self.ty(span, hir::TyKind::Err));
                let generics = self.lower_generics(generics, ImplTraitContext::disallowed());
                hir::ItemKind::TyAlias(ty, generics)
            }
            ItemKind::Enum(ref enum_definition, ref generics) => hir::ItemKind::Enum(
                hir::EnumDef {
                    variants: self.arena.alloc_from_iter(
                        enum_definition.variants.iter().map(|x| self.lower_variant(x)),
                    ),
                },
                self.lower_generics(generics, ImplTraitContext::disallowed()),
            ),
            ItemKind::Struct(ref struct_def, ref generics) => {
                let struct_def = self.lower_variant_data(hir_id, struct_def);
                hir::ItemKind::Struct(
                    struct_def,
                    self.lower_generics(generics, ImplTraitContext::disallowed()),
                )
            }
            ItemKind::Union(ref vdata, ref generics) => {
                let vdata = self.lower_variant_data(hir_id, vdata);
                hir::ItemKind::Union(
                    vdata,
                    self.lower_generics(generics, ImplTraitContext::disallowed()),
                )
            }
            ItemKind::Impl(box ImplKind {
                unsafety,
                polarity,
                defaultness,
                constness,
                generics: ref ast_generics,
                of_trait: ref trait_ref,
                self_ty: ref ty,
                items: ref impl_items,
            }) => {
                // Lower the "impl header" first. This ordering is important
                // for in-band lifetimes! Consider `'a` here:
                //
                //     impl Foo<'a> for u32 {
                //         fn method(&'a self) { .. }
                //     }
                //
                // Because we start by lowering the `Foo<'a> for u32`
                // part, we will add `'a` to the list of generics on
                // the impl. When we then encounter it later in the
                // method, it will not be considered an in-band
                // lifetime to be added, but rather a reference to a
                // parent lifetime.
                let lowered_trait_def_id = self.lower_node_id(id).expect_owner();
                let (generics, (trait_ref, lowered_ty)) = self.add_in_band_defs(
                    ast_generics,
                    lowered_trait_def_id,
                    AnonymousLifetimeMode::CreateParameter,
                    |this, _| {
                        let trait_ref = trait_ref.as_ref().map(|trait_ref| {
                            this.lower_trait_ref(trait_ref, ImplTraitContext::disallowed())
                        });

                        if let Some(ref trait_ref) = trait_ref {
                            if let Res::Def(DefKind::Trait, def_id) = trait_ref.path.res {
                                this.trait_impls
                                    .entry(def_id)
                                    .or_default()
                                    .push(lowered_trait_def_id);
                            }
                        }

                        let lowered_ty = this.lower_ty(ty, ImplTraitContext::disallowed());

                        (trait_ref, lowered_ty)
                    },
                );

                let new_impl_items =
                    self.with_in_scope_lifetime_defs(&ast_generics.params, |this| {
                        this.arena.alloc_from_iter(
                            impl_items.iter().map(|item| this.lower_impl_item_ref(item)),
                        )
                    });

                // `defaultness.has_value()` is never called for an `impl`, always `true` in order
                // to not cause an assertion failure inside the `lower_defaultness` function.
                let has_val = true;
                let (defaultness, defaultness_span) = self.lower_defaultness(defaultness, has_val);
                hir::ItemKind::Impl(hir::Impl {
                    unsafety: self.lower_unsafety(unsafety),
                    polarity,
                    defaultness,
                    defaultness_span,
                    constness: self.lower_constness(constness),
                    generics,
                    of_trait: trait_ref,
                    self_ty: lowered_ty,
                    items: new_impl_items,
                })
            }
            ItemKind::Trait(box TraitKind(
                is_auto,
                unsafety,
                ref generics,
                ref bounds,
                ref items,
            )) => {
                let bounds = self.lower_param_bounds(bounds, ImplTraitContext::disallowed());
                let items = self
                    .arena
                    .alloc_from_iter(items.iter().map(|item| self.lower_trait_item_ref(item)));
                hir::ItemKind::Trait(
                    is_auto,
                    self.lower_unsafety(unsafety),
                    self.lower_generics(generics, ImplTraitContext::disallowed()),
                    bounds,
                    items,
                )
            }
            ItemKind::TraitAlias(ref generics, ref bounds) => hir::ItemKind::TraitAlias(
                self.lower_generics(generics, ImplTraitContext::disallowed()),
                self.lower_param_bounds(bounds, ImplTraitContext::disallowed()),
            ),
            ItemKind::MacroDef(..) | ItemKind::MacCall(..) => {
                panic!("`TyMac` should have been expanded by now")
            }
        }
    }

    fn lower_const_item(
        &mut self,
        ty: &Ty,
        span: Span,
        body: Option<&Expr>,
    ) -> (&'hir hir::Ty<'hir>, hir::BodyId) {
        let mut capturable_lifetimes;
        let itctx = if self.sess.features_untracked().impl_trait_in_bindings {
            capturable_lifetimes = FxHashSet::default();
            ImplTraitContext::OtherOpaqueTy {
                capturable_lifetimes: &mut capturable_lifetimes,
                origin: hir::OpaqueTyOrigin::Misc,
            }
        } else {
            ImplTraitContext::Disallowed(ImplTraitPosition::Binding)
        };
        let ty = self.lower_ty(ty, itctx);
        (ty, self.lower_const_body(span, body))
    }

    fn lower_use_tree(
        &mut self,
        tree: &UseTree,
        prefix: &Path,
        id: NodeId,
        vis: &mut hir::Visibility<'hir>,
        ident: &mut Ident,
        attrs: Option<&'hir [Attribute]>,
    ) -> hir::ItemKind<'hir> {
        debug!("lower_use_tree(tree={:?})", tree);
        debug!("lower_use_tree: vis = {:?}", vis);

        let path = &tree.prefix;
        let segments = prefix.segments.iter().chain(path.segments.iter()).cloned().collect();

        match tree.kind {
            UseTreeKind::Simple(rename, id1, id2) => {
                *ident = tree.ident();

                // First, apply the prefix to the path.
                let mut path = Path { segments, span: path.span, tokens: None };

                // Correctly resolve `self` imports.
                if path.segments.len() > 1
                    && path.segments.last().unwrap().ident.name == kw::SelfLower
                {
                    let _ = path.segments.pop();
                    if rename.is_none() {
                        *ident = path.segments.last().unwrap().ident;
                    }
                }

                let mut resolutions = self.expect_full_res_from_use(id);
                // We want to return *something* from this function, so hold onto the first item
                // for later.
                let ret_res = self.lower_res(resolutions.next().unwrap_or(Res::Err));

                // Here, we are looping over namespaces, if they exist for the definition
                // being imported. We only handle type and value namespaces because we
                // won't be dealing with macros in the rest of the compiler.
                // Essentially a single `use` which imports two names is desugared into
                // two imports.
                for (res, &new_node_id) in resolutions.zip([id1, id2].iter()) {
                    let ident = *ident;
                    let mut path = path.clone();
                    for seg in &mut path.segments {
                        seg.id = self.resolver.next_node_id();
                    }
                    let span = path.span;

                    self.with_hir_id_owner(new_node_id, |this| {
                        let new_id = this.lower_node_id(new_node_id);
                        let res = this.lower_res(res);
                        let path = this.lower_path_extra(res, &path, ParamMode::Explicit, None);
                        let kind = hir::ItemKind::Use(path, hir::UseKind::Single);
                        let vis = this.rebuild_vis(&vis);
                        if let Some(attrs) = attrs {
                            this.attrs.insert(new_id, attrs);
                        }

                        this.insert_item(hir::Item {
                            def_id: new_id.expect_owner(),
                            ident,
                            kind,
                            vis,
                            span,
                        });
                    });
                }

                let path = self.lower_path_extra(ret_res, &path, ParamMode::Explicit, None);
                hir::ItemKind::Use(path, hir::UseKind::Single)
            }
            UseTreeKind::Glob => {
                let path = self.lower_path(
                    id,
                    &Path { segments, span: path.span, tokens: None },
                    ParamMode::Explicit,
                );
                hir::ItemKind::Use(path, hir::UseKind::Glob)
            }
            UseTreeKind::Nested(ref trees) => {
                // Nested imports are desugared into simple imports.
                // So, if we start with
                //
                // ```
                // pub(x) use foo::{a, b};
                // ```
                //
                // we will create three items:
                //
                // ```
                // pub(x) use foo::a;
                // pub(x) use foo::b;
                // pub(x) use foo::{}; // <-- this is called the `ListStem`
                // ```
                //
                // The first two are produced by recursively invoking
                // `lower_use_tree` (and indeed there may be things
                // like `use foo::{a::{b, c}}` and so forth).  They
                // wind up being directly added to
                // `self.items`. However, the structure of this
                // function also requires us to return one item, and
                // for that we return the `{}` import (called the
                // `ListStem`).

                let prefix = Path { segments, span: prefix.span.to(path.span), tokens: None };

                // Add all the nested `PathListItem`s to the HIR.
                for &(ref use_tree, id) in trees {
                    let new_hir_id = self.lower_node_id(id);

                    let mut prefix = prefix.clone();

                    // Give the segments new node-ids since they are being cloned.
                    for seg in &mut prefix.segments {
                        seg.id = self.resolver.next_node_id();
                    }

                    // Each `use` import is an item and thus are owners of the
                    // names in the path. Up to this point the nested import is
                    // the current owner, since we want each desugared import to
                    // own its own names, we have to adjust the owner before
                    // lowering the rest of the import.
                    self.with_hir_id_owner(id, |this| {
                        let mut vis = this.rebuild_vis(&vis);
                        let mut ident = *ident;

                        let kind =
                            this.lower_use_tree(use_tree, &prefix, id, &mut vis, &mut ident, attrs);
                        if let Some(attrs) = attrs {
                            this.attrs.insert(new_hir_id, attrs);
                        }

                        this.insert_item(hir::Item {
                            def_id: new_hir_id.expect_owner(),
                            ident,
                            kind,
                            vis,
                            span: use_tree.span,
                        });
                    });
                }

                // Subtle and a bit hacky: we lower the privacy level
                // of the list stem to "private" most of the time, but
                // not for "restricted" paths. The key thing is that
                // we don't want it to stay as `pub` (with no caveats)
                // because that affects rustdoc and also the lints
                // about `pub` items. But we can't *always* make it
                // private -- particularly not for restricted paths --
                // because it contains node-ids that would then be
                // unused, failing the check that HirIds are "densely
                // assigned".
                match vis.node {
                    hir::VisibilityKind::Public
                    | hir::VisibilityKind::Crate(_)
                    | hir::VisibilityKind::Inherited => {
                        *vis = respan(prefix.span.shrink_to_lo(), hir::VisibilityKind::Inherited);
                    }
                    hir::VisibilityKind::Restricted { .. } => {
                        // Do nothing here, as described in the comment on the match.
                    }
                }

                let res = self.expect_full_res_from_use(id).next().unwrap_or(Res::Err);
                let res = self.lower_res(res);
                let path = self.lower_path_extra(res, &prefix, ParamMode::Explicit, None);
                hir::ItemKind::Use(path, hir::UseKind::ListStem)
            }
        }
    }

    /// Paths like the visibility path in `pub(super) use foo::{bar, baz}` are repeated
    /// many times in the HIR tree; for each occurrence, we need to assign distinct
    /// `NodeId`s. (See, e.g., #56128.)
    fn rebuild_use_path(&mut self, path: &hir::Path<'hir>) -> &'hir hir::Path<'hir> {
        debug!("rebuild_use_path(path = {:?})", path);
        let segments =
            self.arena.alloc_from_iter(path.segments.iter().map(|seg| hir::PathSegment {
                ident: seg.ident,
                hir_id: seg.hir_id.map(|_| self.next_id()),
                res: seg.res,
                args: None,
                infer_args: seg.infer_args,
            }));
        self.arena.alloc(hir::Path { span: path.span, res: path.res, segments })
    }

    fn rebuild_vis(&mut self, vis: &hir::Visibility<'hir>) -> hir::Visibility<'hir> {
        let vis_kind = match vis.node {
            hir::VisibilityKind::Public => hir::VisibilityKind::Public,
            hir::VisibilityKind::Crate(sugar) => hir::VisibilityKind::Crate(sugar),
            hir::VisibilityKind::Inherited => hir::VisibilityKind::Inherited,
            hir::VisibilityKind::Restricted { ref path, hir_id: _ } => {
                hir::VisibilityKind::Restricted {
                    path: self.rebuild_use_path(path),
                    hir_id: self.next_id(),
                }
            }
        };
        respan(vis.span, vis_kind)
    }

    fn lower_foreign_item(&mut self, i: &ForeignItem) -> hir::ForeignItem<'hir> {
        let hir_id = self.lower_node_id(i.id);
        let def_id = hir_id.expect_owner();
        self.lower_attrs(hir_id, &i.attrs);
        hir::ForeignItem {
            def_id,
            ident: i.ident,
            kind: match i.kind {
                ForeignItemKind::Fn(box FnKind(_, ref sig, ref generics, _)) => {
                    let fdec = &sig.decl;
                    let (generics, (fn_dec, fn_args)) = self.add_in_band_defs(
                        generics,
                        def_id,
                        AnonymousLifetimeMode::PassThrough,
                        |this, _| {
                            (
                                // Disallow `impl Trait` in foreign items.
                                this.lower_fn_decl(fdec, None, false, None),
                                this.lower_fn_params_to_names(fdec),
                            )
                        },
                    );

                    hir::ForeignItemKind::Fn(fn_dec, fn_args, generics)
                }
                ForeignItemKind::Static(ref t, m, _) => {
                    let ty = self.lower_ty(t, ImplTraitContext::disallowed());
                    hir::ForeignItemKind::Static(ty, m)
                }
                ForeignItemKind::TyAlias(..) => hir::ForeignItemKind::Type,
                ForeignItemKind::MacCall(_) => panic!("macro shouldn't exist here"),
            },
            vis: self.lower_visibility(&i.vis, None),
            span: i.span,
        }
    }

    fn lower_foreign_item_ref(&mut self, i: &ForeignItem) -> hir::ForeignItemRef<'hir> {
        hir::ForeignItemRef {
            id: hir::ForeignItemId { def_id: self.lower_node_id(i.id).expect_owner() },
            ident: i.ident,
            span: i.span,
            vis: self.lower_visibility(&i.vis, Some(i.id)),
        }
    }

    fn lower_global_asm(&mut self, ga: &GlobalAsm) -> &'hir hir::GlobalAsm {
        self.arena.alloc(hir::GlobalAsm { asm: ga.asm })
    }

    fn lower_variant(&mut self, v: &Variant) -> hir::Variant<'hir> {
        let id = self.lower_node_id(v.id);
        self.lower_attrs(id, &v.attrs);
        hir::Variant {
            id,
            data: self.lower_variant_data(id, &v.data),
            disr_expr: v.disr_expr.as_ref().map(|e| self.lower_anon_const(e)),
            ident: v.ident,
            span: v.span,
        }
    }

    fn lower_variant_data(
        &mut self,
        parent_id: hir::HirId,
        vdata: &VariantData,
    ) -> hir::VariantData<'hir> {
        match *vdata {
            VariantData::Struct(ref fields, recovered) => hir::VariantData::Struct(
                self.arena
                    .alloc_from_iter(fields.iter().enumerate().map(|f| self.lower_struct_field(f))),
                recovered,
            ),
            VariantData::Tuple(ref fields, id) => {
                let ctor_id = self.lower_node_id(id);
                self.alias_attrs(ctor_id, parent_id);
                hir::VariantData::Tuple(
                    self.arena.alloc_from_iter(
                        fields.iter().enumerate().map(|f| self.lower_struct_field(f)),
                    ),
                    ctor_id,
                )
            }
            VariantData::Unit(id) => {
                let ctor_id = self.lower_node_id(id);
                self.alias_attrs(ctor_id, parent_id);
                hir::VariantData::Unit(ctor_id)
            }
        }
    }

    fn lower_struct_field(&mut self, (index, f): (usize, &StructField)) -> hir::StructField<'hir> {
        let ty = if let TyKind::Path(ref qself, ref path) = f.ty.kind {
            let t = self.lower_path_ty(
                &f.ty,
                qself,
                path,
                ParamMode::ExplicitNamed, // no `'_` in declarations (Issue #61124)
                ImplTraitContext::disallowed(),
            );
            self.arena.alloc(t)
        } else {
            self.lower_ty(&f.ty, ImplTraitContext::disallowed())
        };
        let hir_id = self.lower_node_id(f.id);
        self.lower_attrs(hir_id, &f.attrs);
        hir::StructField {
            span: f.span,
            hir_id,
            ident: match f.ident {
                Some(ident) => ident,
                // FIXME(jseyfried): positional field hygiene.
                None => Ident::new(sym::integer(index), f.span),
            },
            vis: self.lower_visibility(&f.vis, None),
            ty,
        }
    }

    fn lower_trait_item(&mut self, i: &AssocItem) -> hir::TraitItem<'hir> {
        let hir_id = self.lower_node_id(i.id);
        let trait_item_def_id = hir_id.expect_owner();

        let (generics, kind) = match i.kind {
            AssocItemKind::Const(_, ref ty, ref default) => {
                let ty = self.lower_ty(ty, ImplTraitContext::disallowed());
                let body = default.as_ref().map(|x| self.lower_const_body(i.span, Some(x)));
                (hir::Generics::empty(), hir::TraitItemKind::Const(ty, body))
            }
            AssocItemKind::Fn(box FnKind(_, ref sig, ref generics, None)) => {
                let names = self.lower_fn_params_to_names(&sig.decl);
                let (generics, sig) =
                    self.lower_method_sig(generics, sig, trait_item_def_id, false, None, i.id);
                (generics, hir::TraitItemKind::Fn(sig, hir::TraitFn::Required(names)))
            }
            AssocItemKind::Fn(box FnKind(_, ref sig, ref generics, Some(ref body))) => {
                let body_id = self.lower_fn_body_block(i.span, &sig.decl, Some(body));
                let (generics, sig) =
                    self.lower_method_sig(generics, sig, trait_item_def_id, false, None, i.id);
                (generics, hir::TraitItemKind::Fn(sig, hir::TraitFn::Provided(body_id)))
            }
            AssocItemKind::TyAlias(box TyAliasKind(_, ref generics, ref bounds, ref default)) => {
                let ty = default.as_ref().map(|x| self.lower_ty(x, ImplTraitContext::disallowed()));
                let generics = self.lower_generics(generics, ImplTraitContext::disallowed());
                let kind = hir::TraitItemKind::Type(
                    self.lower_param_bounds(bounds, ImplTraitContext::disallowed()),
                    ty,
                );

                (generics, kind)
            }
            AssocItemKind::MacCall(..) => panic!("macro item shouldn't exist at this point"),
        };

        self.lower_attrs(hir_id, &i.attrs);
        hir::TraitItem { def_id: trait_item_def_id, ident: i.ident, generics, kind, span: i.span }
    }

    fn lower_trait_item_ref(&mut self, i: &AssocItem) -> hir::TraitItemRef {
        let (kind, has_default) = match &i.kind {
            AssocItemKind::Const(_, _, default) => (hir::AssocItemKind::Const, default.is_some()),
            AssocItemKind::TyAlias(box TyAliasKind(_, _, _, default)) => {
                (hir::AssocItemKind::Type, default.is_some())
            }
            AssocItemKind::Fn(box FnKind(_, sig, _, default)) => {
                (hir::AssocItemKind::Fn { has_self: sig.decl.has_self() }, default.is_some())
            }
            AssocItemKind::MacCall(..) => unimplemented!(),
        };
        let id = hir::TraitItemId { def_id: self.lower_node_id(i.id).expect_owner() };
        let defaultness = hir::Defaultness::Default { has_value: has_default };
        hir::TraitItemRef { id, ident: i.ident, span: i.span, defaultness, kind }
    }

    /// Construct `ExprKind::Err` for the given `span`.
    crate fn expr_err(&mut self, span: Span) -> hir::Expr<'hir> {
        self.expr(span, hir::ExprKind::Err, AttrVec::new())
    }

    fn lower_impl_item(&mut self, i: &AssocItem) -> hir::ImplItem<'hir> {
        let impl_item_def_id = self.resolver.local_def_id(i.id);

        let (generics, kind) = match &i.kind {
            AssocItemKind::Const(_, ty, expr) => {
                let ty = self.lower_ty(ty, ImplTraitContext::disallowed());
                (
                    hir::Generics::empty(),
                    hir::ImplItemKind::Const(ty, self.lower_const_body(i.span, expr.as_deref())),
                )
            }
            AssocItemKind::Fn(box FnKind(_, sig, generics, body)) => {
                self.current_item = Some(i.span);
                let asyncness = sig.header.asyncness;
                let body_id =
                    self.lower_maybe_async_body(i.span, &sig.decl, asyncness, body.as_deref());
                let impl_trait_return_allow = !self.is_in_trait_impl;
                let (generics, sig) = self.lower_method_sig(
                    generics,
                    sig,
                    impl_item_def_id,
                    impl_trait_return_allow,
                    asyncness.opt_return_id(),
                    i.id,
                );

                (generics, hir::ImplItemKind::Fn(sig, body_id))
            }
            AssocItemKind::TyAlias(box TyAliasKind(_, generics, _, ty)) => {
                let generics = self.lower_generics(generics, ImplTraitContext::disallowed());
                let kind = match ty {
                    None => {
                        let ty = self.arena.alloc(self.ty(i.span, hir::TyKind::Err));
                        hir::ImplItemKind::TyAlias(ty)
                    }
                    Some(ty) => {
                        let ty = self.lower_ty(
                            ty,
                            ImplTraitContext::OtherOpaqueTy {
                                capturable_lifetimes: &mut FxHashSet::default(),
                                origin: hir::OpaqueTyOrigin::Misc,
                            },
                        );
                        hir::ImplItemKind::TyAlias(ty)
                    }
                };
                (generics, kind)
            }
            AssocItemKind::MacCall(..) => panic!("`TyMac` should have been expanded by now"),
        };

        // Since `default impl` is not yet implemented, this is always true in impls.
        let has_value = true;
        let (defaultness, _) = self.lower_defaultness(i.kind.defaultness(), has_value);
        let hir_id = self.lower_node_id(i.id);
        self.lower_attrs(hir_id, &i.attrs);
        hir::ImplItem {
            def_id: hir_id.expect_owner(),
            ident: i.ident,
            generics,
            vis: self.lower_visibility(&i.vis, None),
            defaultness,
            kind,
            span: i.span,
        }
    }

    fn lower_impl_item_ref(&mut self, i: &AssocItem) -> hir::ImplItemRef<'hir> {
        // Since `default impl` is not yet implemented, this is always true in impls.
        let has_value = true;
        let (defaultness, _) = self.lower_defaultness(i.kind.defaultness(), has_value);
        hir::ImplItemRef {
            id: hir::ImplItemId { def_id: self.lower_node_id(i.id).expect_owner() },
            ident: i.ident,
            span: i.span,
            vis: self.lower_visibility(&i.vis, Some(i.id)),
            defaultness,
            kind: match &i.kind {
                AssocItemKind::Const(..) => hir::AssocItemKind::Const,
                AssocItemKind::TyAlias(..) => hir::AssocItemKind::Type,
                AssocItemKind::Fn(box FnKind(_, sig, ..)) => {
                    hir::AssocItemKind::Fn { has_self: sig.decl.has_self() }
                }
                AssocItemKind::MacCall(..) => unimplemented!(),
            },
        }
    }

    /// If an `explicit_owner` is given, this method allocates the `HirId` in
    /// the address space of that item instead of the item currently being
    /// lowered. This can happen during `lower_impl_item_ref()` where we need to
    /// lower a `Visibility` value although we haven't lowered the owning
    /// `ImplItem` in question yet.
    fn lower_visibility(
        &mut self,
        v: &Visibility,
        explicit_owner: Option<NodeId>,
    ) -> hir::Visibility<'hir> {
        let node = match v.kind {
            VisibilityKind::Public => hir::VisibilityKind::Public,
            VisibilityKind::Crate(sugar) => hir::VisibilityKind::Crate(sugar),
            VisibilityKind::Restricted { ref path, id } => {
                debug!("lower_visibility: restricted path id = {:?}", id);
                let lowered_id = if let Some(owner) = explicit_owner {
                    self.lower_node_id_with_owner(id, owner)
                } else {
                    self.lower_node_id(id)
                };
                let res = self.expect_full_res(id);
                let res = self.lower_res(res);
                hir::VisibilityKind::Restricted {
                    path: self.lower_path_extra(res, path, ParamMode::Explicit, explicit_owner),
                    hir_id: lowered_id,
                }
            }
            VisibilityKind::Inherited => hir::VisibilityKind::Inherited,
        };
        respan(v.span, node)
    }

    fn lower_defaultness(
        &self,
        d: Defaultness,
        has_value: bool,
    ) -> (hir::Defaultness, Option<Span>) {
        match d {
            Defaultness::Default(sp) => (hir::Defaultness::Default { has_value }, Some(sp)),
            Defaultness::Final => {
                assert!(has_value);
                (hir::Defaultness::Final, None)
            }
        }
    }

    fn record_body(
        &mut self,
        params: &'hir [hir::Param<'hir>],
        value: hir::Expr<'hir>,
    ) -> hir::BodyId {
        let body = hir::Body { generator_kind: self.generator_kind, params, value };
        let id = body.id();
        self.bodies.insert(id, body);
        id
    }

    pub(super) fn lower_body(
        &mut self,
        f: impl FnOnce(&mut Self) -> (&'hir [hir::Param<'hir>], hir::Expr<'hir>),
    ) -> hir::BodyId {
        let prev_gen_kind = self.generator_kind.take();
        let task_context = self.task_context.take();
        let (parameters, result) = f(self);
        let body_id = self.record_body(parameters, result);
        self.task_context = task_context;
        self.generator_kind = prev_gen_kind;
        body_id
    }

    fn lower_param(&mut self, param: &Param) -> hir::Param<'hir> {
        let hir_id = self.lower_node_id(param.id);
        self.lower_attrs(hir_id, &param.attrs);
        hir::Param {
            hir_id,
            pat: self.lower_pat(&param.pat),
            ty_span: param.ty.span,
            span: param.span,
        }
    }

    pub(super) fn lower_fn_body(
        &mut self,
        decl: &FnDecl,
        body: impl FnOnce(&mut Self) -> hir::Expr<'hir>,
    ) -> hir::BodyId {
        self.lower_body(|this| {
            (
                this.arena.alloc_from_iter(decl.inputs.iter().map(|x| this.lower_param(x))),
                body(this),
            )
        })
    }

    fn lower_fn_body_block(
        &mut self,
        span: Span,
        decl: &FnDecl,
        body: Option<&Block>,
    ) -> hir::BodyId {
        self.lower_fn_body(decl, |this| this.lower_block_expr_opt(span, body))
    }

    fn lower_block_expr_opt(&mut self, span: Span, block: Option<&Block>) -> hir::Expr<'hir> {
        match block {
            Some(block) => self.lower_block_expr(block),
            None => self.expr_err(span),
        }
    }

    pub(super) fn lower_const_body(&mut self, span: Span, expr: Option<&Expr>) -> hir::BodyId {
        self.lower_body(|this| {
            (
                &[],
                match expr {
                    Some(expr) => this.lower_expr_mut(expr),
                    None => this.expr_err(span),
                },
            )
        })
    }

    fn lower_maybe_async_body(
        &mut self,
        span: Span,
        decl: &FnDecl,
        asyncness: Async,
        body: Option<&Block>,
    ) -> hir::BodyId {
        let closure_id = match asyncness {
            Async::Yes { closure_id, .. } => closure_id,
            Async::No => return self.lower_fn_body_block(span, decl, body),
        };

        self.lower_body(|this| {
            let mut parameters: Vec<hir::Param<'_>> = Vec::new();
            let mut statements: Vec<hir::Stmt<'_>> = Vec::new();

            // Async function parameters are lowered into the closure body so that they are
            // captured and so that the drop order matches the equivalent non-async functions.
            //
            // from:
            //
            //     async fn foo(<pattern>: <ty>, <pattern>: <ty>, <pattern>: <ty>) {
            //         <body>
            //     }
            //
            // into:
            //
            //     fn foo(__arg0: <ty>, __arg1: <ty>, __arg2: <ty>) {
            //       async move {
            //         let __arg2 = __arg2;
            //         let <pattern> = __arg2;
            //         let __arg1 = __arg1;
            //         let <pattern> = __arg1;
            //         let __arg0 = __arg0;
            //         let <pattern> = __arg0;
            //         drop-temps { <body> } // see comments later in fn for details
            //       }
            //     }
            //
            // If `<pattern>` is a simple ident, then it is lowered to a single
            // `let <pattern> = <pattern>;` statement as an optimization.
            //
            // Note that the body is embedded in `drop-temps`; an
            // equivalent desugaring would be `return { <body>
            // };`. The key point is that we wish to drop all the
            // let-bound variables and temporaries created in the body
            // (and its tail expression!) before we drop the
            // parameters (c.f. rust-lang/rust#64512).
            for (index, parameter) in decl.inputs.iter().enumerate() {
                let parameter = this.lower_param(parameter);
                let span = parameter.pat.span;

                // Check if this is a binding pattern, if so, we can optimize and avoid adding a
                // `let <pat> = __argN;` statement. In this case, we do not rename the parameter.
                let (ident, is_simple_parameter) = match parameter.pat.kind {
                    hir::PatKind::Binding(
                        hir::BindingAnnotation::Unannotated | hir::BindingAnnotation::Mutable,
                        _,
                        ident,
                        _,
                    ) => (ident, true),
                    // For `ref mut` or wildcard arguments, we can't reuse the binding, but
                    // we can keep the same name for the parameter.
                    // This lets rustdoc render it correctly in documentation.
                    hir::PatKind::Binding(_, _, ident, _) => (ident, false),
                    hir::PatKind::Wild => {
                        (Ident::with_dummy_span(rustc_span::symbol::kw::Underscore), false)
                    }
                    _ => {
                        // Replace the ident for bindings that aren't simple.
                        let name = format!("__arg{}", index);
                        let ident = Ident::from_str(&name);

                        (ident, false)
                    }
                };

                let desugared_span = this.mark_span_with_reason(DesugaringKind::Async, span, None);

                // Construct a parameter representing `__argN: <ty>` to replace the parameter of the
                // async function.
                //
                // If this is the simple case, this parameter will end up being the same as the
                // original parameter, but with a different pattern id.
                let stmt_attrs = this.attrs.get(&parameter.hir_id).copied();
                let (new_parameter_pat, new_parameter_id) = this.pat_ident(desugared_span, ident);
                let new_parameter = hir::Param {
                    hir_id: parameter.hir_id,
                    pat: new_parameter_pat,
                    ty_span: parameter.ty_span,
                    span: parameter.span,
                };

                if is_simple_parameter {
                    // If this is the simple case, then we only insert one statement that is
                    // `let <pat> = <pat>;`. We re-use the original argument's pattern so that
                    // `HirId`s are densely assigned.
                    let expr = this.expr_ident(desugared_span, ident, new_parameter_id);
                    let stmt = this.stmt_let_pat(
                        stmt_attrs,
                        desugared_span,
                        Some(expr),
                        parameter.pat,
                        hir::LocalSource::AsyncFn,
                    );
                    statements.push(stmt);
                } else {
                    // If this is not the simple case, then we construct two statements:
                    //
                    // ```
                    // let __argN = __argN;
                    // let <pat> = __argN;
                    // ```
                    //
                    // The first statement moves the parameter into the closure and thus ensures
                    // that the drop order is correct.
                    //
                    // The second statement creates the bindings that the user wrote.

                    // Construct the `let mut __argN = __argN;` statement. It must be a mut binding
                    // because the user may have specified a `ref mut` binding in the next
                    // statement.
                    let (move_pat, move_id) = this.pat_ident_binding_mode(
                        desugared_span,
                        ident,
                        hir::BindingAnnotation::Mutable,
                    );
                    let move_expr = this.expr_ident(desugared_span, ident, new_parameter_id);
                    let move_stmt = this.stmt_let_pat(
                        None,
                        desugared_span,
                        Some(move_expr),
                        move_pat,
                        hir::LocalSource::AsyncFn,
                    );

                    // Construct the `let <pat> = __argN;` statement. We re-use the original
                    // parameter's pattern so that `HirId`s are densely assigned.
                    let pattern_expr = this.expr_ident(desugared_span, ident, move_id);
                    let pattern_stmt = this.stmt_let_pat(
                        stmt_attrs,
                        desugared_span,
                        Some(pattern_expr),
                        parameter.pat,
                        hir::LocalSource::AsyncFn,
                    );

                    statements.push(move_stmt);
                    statements.push(pattern_stmt);
                };

                parameters.push(new_parameter);
            }

            let body_span = body.map_or(span, |b| b.span);
            let async_expr = this.make_async_expr(
                CaptureBy::Value,
                closure_id,
                None,
                body_span,
                hir::AsyncGeneratorKind::Fn,
                |this| {
                    // Create a block from the user's function body:
                    let user_body = this.lower_block_expr_opt(body_span, body);

                    // Transform into `drop-temps { <user-body> }`, an expression:
                    let desugared_span =
                        this.mark_span_with_reason(DesugaringKind::Async, user_body.span, None);
                    let user_body = this.expr_drop_temps(
                        desugared_span,
                        this.arena.alloc(user_body),
                        AttrVec::new(),
                    );

                    // As noted above, create the final block like
                    //
                    // ```
                    // {
                    //   let $param_pattern = $raw_param;
                    //   ...
                    //   drop-temps { <user-body> }
                    // }
                    // ```
                    let body = this.block_all(
                        desugared_span,
                        this.arena.alloc_from_iter(statements),
                        Some(user_body),
                    );

                    this.expr_block(body, AttrVec::new())
                },
            );

            (
                this.arena.alloc_from_iter(parameters),
                this.expr(body_span, async_expr, AttrVec::new()),
            )
        })
    }

    fn lower_method_sig(
        &mut self,
        generics: &Generics,
        sig: &FnSig,
        fn_def_id: LocalDefId,
        impl_trait_return_allow: bool,
        is_async: Option<NodeId>,
        id: NodeId,
    ) -> (hir::Generics<'hir>, hir::FnSig<'hir>) {
        let header = self.lower_fn_header(sig.header, sig.span, id);
        let (generics, decl) = self.add_in_band_defs(
            generics,
            fn_def_id,
            AnonymousLifetimeMode::PassThrough,
            |this, idty| {
                this.lower_fn_decl(
                    &sig.decl,
                    Some((fn_def_id.to_def_id(), idty)),
                    impl_trait_return_allow,
                    is_async,
                )
            },
        );
        (generics, hir::FnSig { header, decl, span: sig.span })
    }

    fn lower_fn_header(&mut self, h: FnHeader, span: Span, id: NodeId) -> hir::FnHeader {
        hir::FnHeader {
            unsafety: self.lower_unsafety(h.unsafety),
            asyncness: self.lower_asyncness(h.asyncness),
            constness: self.lower_constness(h.constness),
            abi: self.lower_extern(h.ext, span, id),
        }
    }

    pub(super) fn lower_abi(&mut self, abi: StrLit) -> abi::Abi {
        abi::lookup(&abi.symbol_unescaped.as_str()).unwrap_or_else(|| {
            self.error_on_invalid_abi(abi);
            abi::Abi::Rust
        })
    }

    pub(super) fn lower_extern(&mut self, ext: Extern, span: Span, id: NodeId) -> abi::Abi {
        match ext {
            Extern::None => abi::Abi::Rust,
            Extern::Implicit => {
                self.maybe_lint_missing_abi(span, id, abi::Abi::C);
                abi::Abi::C
            }
            Extern::Explicit(abi) => self.lower_abi(abi),
        }
    }

    fn error_on_invalid_abi(&self, abi: StrLit) {
        struct_span_err!(self.sess, abi.span, E0703, "invalid ABI: found `{}`", abi.symbol)
            .span_label(abi.span, "invalid ABI")
            .help(&format!("valid ABIs: {}", abi::all_names().join(", ")))
            .emit();
    }

    fn lower_asyncness(&mut self, a: Async) -> hir::IsAsync {
        match a {
            Async::Yes { .. } => hir::IsAsync::Async,
            Async::No => hir::IsAsync::NotAsync,
        }
    }

    fn lower_constness(&mut self, c: Const) -> hir::Constness {
        match c {
            Const::Yes(_) => hir::Constness::Const,
            Const::No => hir::Constness::NotConst,
        }
    }

    pub(super) fn lower_unsafety(&mut self, u: Unsafe) -> hir::Unsafety {
        match u {
            Unsafe::Yes(_) => hir::Unsafety::Unsafe,
            Unsafe::No => hir::Unsafety::Normal,
        }
    }

    pub(super) fn lower_generics_mut(
        &mut self,
        generics: &Generics,
        itctx: ImplTraitContext<'_, 'hir>,
    ) -> GenericsCtor<'hir> {
        // Collect `?Trait` bounds in where clause and move them to parameter definitions.
        // FIXME: this could probably be done with less rightward drift. It also looks like two
        // control paths where `report_error` is called are the only paths that advance to after the
        // match statement, so the error reporting could probably just be moved there.
        let mut add_bounds: NodeMap<Vec<_>> = Default::default();
        for pred in &generics.where_clause.predicates {
            if let WherePredicate::BoundPredicate(ref bound_pred) = *pred {
                'next_bound: for bound in &bound_pred.bounds {
                    if let GenericBound::Trait(_, TraitBoundModifier::Maybe) = *bound {
                        let report_error = |this: &mut Self| {
                            this.diagnostic().span_err(
                                bound_pred.bounded_ty.span,
                                "`?Trait` bounds are only permitted at the \
                                 point where a type parameter is declared",
                            );
                        };
                        // Check if the where clause type is a plain type parameter.
                        match bound_pred.bounded_ty.kind {
                            TyKind::Path(None, ref path)
                                if path.segments.len() == 1
                                    && bound_pred.bound_generic_params.is_empty() =>
                            {
                                if let Some(Res::Def(DefKind::TyParam, def_id)) = self
                                    .resolver
                                    .get_partial_res(bound_pred.bounded_ty.id)
                                    .map(|d| d.base_res())
                                {
                                    if let Some(def_id) = def_id.as_local() {
                                        for param in &generics.params {
                                            if let GenericParamKind::Type { .. } = param.kind {
                                                if def_id == self.resolver.local_def_id(param.id) {
                                                    add_bounds
                                                        .entry(param.id)
                                                        .or_default()
                                                        .push(bound.clone());
                                                    continue 'next_bound;
                                                }
                                            }
                                        }
                                    }
                                }
                                report_error(self)
                            }
                            _ => report_error(self),
                        }
                    }
                }
            }
        }

        GenericsCtor {
            params: self.lower_generic_params_mut(&generics.params, &add_bounds, itctx).collect(),
            where_clause: self.lower_where_clause(&generics.where_clause),
            span: generics.span,
        }
    }

    pub(super) fn lower_generics(
        &mut self,
        generics: &Generics,
        itctx: ImplTraitContext<'_, 'hir>,
    ) -> hir::Generics<'hir> {
        let generics_ctor = self.lower_generics_mut(generics, itctx);
        generics_ctor.into_generics(self.arena)
    }

    fn lower_where_clause(&mut self, wc: &WhereClause) -> hir::WhereClause<'hir> {
        self.with_anonymous_lifetime_mode(AnonymousLifetimeMode::ReportError, |this| {
            hir::WhereClause {
                predicates: this.arena.alloc_from_iter(
                    wc.predicates.iter().map(|predicate| this.lower_where_predicate(predicate)),
                ),
                span: wc.span,
            }
        })
    }

    fn lower_where_predicate(&mut self, pred: &WherePredicate) -> hir::WherePredicate<'hir> {
        match *pred {
            WherePredicate::BoundPredicate(WhereBoundPredicate {
                ref bound_generic_params,
                ref bounded_ty,
                ref bounds,
                span,
            }) => {
                self.with_in_scope_lifetime_defs(&bound_generic_params, |this| {
                    hir::WherePredicate::BoundPredicate(hir::WhereBoundPredicate {
                        bound_generic_params: this.lower_generic_params(
                            bound_generic_params,
                            &NodeMap::default(),
                            ImplTraitContext::disallowed(),
                        ),
                        bounded_ty: this.lower_ty(bounded_ty, ImplTraitContext::disallowed()),
                        bounds: this.arena.alloc_from_iter(bounds.iter().filter_map(|bound| {
                            match *bound {
                                // Ignore `?Trait` bounds.
                                // They were copied into type parameters already.
                                GenericBound::Trait(_, TraitBoundModifier::Maybe) => None,
                                _ => Some(
                                    this.lower_param_bound(bound, ImplTraitContext::disallowed()),
                                ),
                            }
                        })),
                        span,
                    })
                })
            }
            WherePredicate::RegionPredicate(WhereRegionPredicate {
                ref lifetime,
                ref bounds,
                span,
            }) => hir::WherePredicate::RegionPredicate(hir::WhereRegionPredicate {
                span,
                lifetime: self.lower_lifetime(lifetime),
                bounds: self.lower_param_bounds(bounds, ImplTraitContext::disallowed()),
            }),
            WherePredicate::EqPredicate(WhereEqPredicate { id, ref lhs_ty, ref rhs_ty, span }) => {
                hir::WherePredicate::EqPredicate(hir::WhereEqPredicate {
                    hir_id: self.lower_node_id(id),
                    lhs_ty: self.lower_ty(lhs_ty, ImplTraitContext::disallowed()),
                    rhs_ty: self.lower_ty(rhs_ty, ImplTraitContext::disallowed()),
                    span,
                })
            }
        }
    }
}

/// Helper struct for delayed construction of Generics.
pub(super) struct GenericsCtor<'hir> {
    pub(super) params: SmallVec<[hir::GenericParam<'hir>; 4]>,
    where_clause: hir::WhereClause<'hir>,
    span: Span,
}

impl<'hir> GenericsCtor<'hir> {
    pub(super) fn into_generics(self, arena: &'hir Arena<'hir>) -> hir::Generics<'hir> {
        hir::Generics {
            params: arena.alloc_from_iter(self.params),
            where_clause: self.where_clause,
            span: self.span,
        }
    }
}
