//! Implementation of lint checking.
//!
//! The lint checking is mostly consolidated into one pass which runs
//! after all other analyses. Throughout compilation, lint warnings
//! can be added via the `add_lint` method on the Session structure. This
//! requires a span and an ID of the node that the lint is being added to. The
//! lint isn't actually emitted at that time because it is unknown what the
//! actual lint level at that location is.
//!
//! To actually emit lint warnings/errors, a separate pass is used.
//! A context keeps track of the current state of all lint levels.
//! Upon entering a node of the ast which can modify the lint settings, the
//! previous lint state is pushed onto a stack and the ast is then recursed
//! upon. As the ast is traversed, this keeps track of the current lint level
//! for all lint attributes.

use crate::{passes::LateLintPassObject, LateContext, LateLintPass, LintStore};
use rustc_ast::ast;
use rustc_ast::walk_list;
use rustc_data_structures::sync::{join, par_iter, ParallelIterator};
use rustc_hir as hir;
use rustc_hir::def_id::{LocalDefId, LOCAL_CRATE};
use rustc_hir::intravisit as hir_visit;
use rustc_hir::intravisit::Visitor;
use rustc_middle::hir::map::Map;
use rustc_middle::ty::{self, TyCtxt};
use rustc_session::lint::LintPass;
use rustc_span::symbol::Symbol;
use rustc_span::Span;

use log::debug;
use std::any::Any;
use std::slice;

/// Extract the `LintStore` from the query context.
/// This function exists because we've erased `LintStore` as `dyn Any` in the context.
crate fn unerased_lint_store(tcx: TyCtxt<'_>) -> &LintStore {
    let store: &dyn Any = &*tcx.lint_store;
    store.downcast_ref().unwrap()
}

macro_rules! lint_callback { ($cx:expr, $f:ident, $($args:expr),*) => ({
    $cx.pass.$f(&$cx.context, $($args),*);
}) }

struct LateContextAndPass<'a, 'tcx, T: LateLintPass<'a, 'tcx>> {
    context: LateContext<'a, 'tcx>,
    pass: T,
}

impl<'a, 'tcx, T: LateLintPass<'a, 'tcx>> LateContextAndPass<'a, 'tcx, T> {
    /// Merge the lints specified by any lint attributes into the
    /// current lint context, call the provided function, then reset the
    /// lints in effect to their previous state.
    fn with_lint_attrs<F>(&mut self, id: hir::HirId, attrs: &'tcx [ast::Attribute], f: F)
    where
        F: FnOnce(&mut Self),
    {
        let prev = self.context.last_node_with_lint_attrs;
        self.context.last_node_with_lint_attrs = id;
        self.enter_attrs(attrs);
        f(self);
        self.exit_attrs(attrs);
        self.context.last_node_with_lint_attrs = prev;
    }

    fn with_param_env<F>(&mut self, id: hir::HirId, f: F)
    where
        F: FnOnce(&mut Self),
    {
        let old_param_env = self.context.param_env;
        self.context.param_env =
            self.context.tcx.param_env(self.context.tcx.hir().local_def_id(id));
        f(self);
        self.context.param_env = old_param_env;
    }

    fn process_mod(&mut self, m: &'tcx hir::Mod<'tcx>, s: Span, n: hir::HirId) {
        lint_callback!(self, check_mod, m, s, n);
        hir_visit::walk_mod(self, m, n);
        lint_callback!(self, check_mod_post, m, s, n);
    }

    fn enter_attrs(&mut self, attrs: &'tcx [ast::Attribute]) {
        debug!("late context: enter_attrs({:?})", attrs);
        lint_callback!(self, enter_lint_attrs, attrs);
    }

    fn exit_attrs(&mut self, attrs: &'tcx [ast::Attribute]) {
        debug!("late context: exit_attrs({:?})", attrs);
        lint_callback!(self, exit_lint_attrs, attrs);
    }
}

impl<'a, 'tcx, T: LateLintPass<'a, 'tcx>> hir_visit::Visitor<'tcx>
    for LateContextAndPass<'a, 'tcx, T>
{
    type Map = Map<'tcx>;

    /// Because lints are scoped lexically, we want to walk nested
    /// items in the context of the outer item, so enable
    /// deep-walking.
    fn nested_visit_map(&mut self) -> hir_visit::NestedVisitorMap<Self::Map> {
        hir_visit::NestedVisitorMap::All(self.context.tcx.hir())
    }

    fn visit_nested_body(&mut self, body: hir::BodyId) {
        let old_tables = self.context.tables;
        self.context.tables = self.context.tcx.body_tables(body);
        let body = self.context.tcx.hir().body(body);
        self.visit_body(body);
        self.context.tables = old_tables;
    }

    fn visit_param(&mut self, param: &'tcx hir::Param<'tcx>) {
        self.with_lint_attrs(param.hir_id, &param.attrs, |cx| {
            lint_callback!(cx, check_param, param);
            hir_visit::walk_param(cx, param);
        });
    }

    fn visit_body(&mut self, body: &'tcx hir::Body<'tcx>) {
        lint_callback!(self, check_body, body);
        hir_visit::walk_body(self, body);
        lint_callback!(self, check_body_post, body);
    }

    fn visit_item(&mut self, it: &'tcx hir::Item<'tcx>) {
        let generics = self.context.generics.take();
        self.context.generics = it.kind.generics();
        self.with_lint_attrs(it.hir_id, &it.attrs, |cx| {
            cx.with_param_env(it.hir_id, |cx| {
                lint_callback!(cx, check_item, it);
                hir_visit::walk_item(cx, it);
                lint_callback!(cx, check_item_post, it);
            });
        });
        self.context.generics = generics;
    }

    fn visit_foreign_item(&mut self, it: &'tcx hir::ForeignItem<'tcx>) {
        self.with_lint_attrs(it.hir_id, &it.attrs, |cx| {
            cx.with_param_env(it.hir_id, |cx| {
                lint_callback!(cx, check_foreign_item, it);
                hir_visit::walk_foreign_item(cx, it);
                lint_callback!(cx, check_foreign_item_post, it);
            });
        })
    }

    fn visit_pat(&mut self, p: &'tcx hir::Pat<'tcx>) {
        lint_callback!(self, check_pat, p);
        hir_visit::walk_pat(self, p);
    }

    fn visit_expr(&mut self, e: &'tcx hir::Expr<'tcx>) {
        self.with_lint_attrs(e.hir_id, &e.attrs, |cx| {
            lint_callback!(cx, check_expr, e);
            hir_visit::walk_expr(cx, e);
            lint_callback!(cx, check_expr_post, e);
        })
    }

    fn visit_stmt(&mut self, s: &'tcx hir::Stmt<'tcx>) {
        // statement attributes are actually just attributes on one of
        // - item
        // - local
        // - expression
        // so we keep track of lint levels there
        lint_callback!(self, check_stmt, s);
        hir_visit::walk_stmt(self, s);
    }

    fn visit_fn(
        &mut self,
        fk: hir_visit::FnKind<'tcx>,
        decl: &'tcx hir::FnDecl<'tcx>,
        body_id: hir::BodyId,
        span: Span,
        id: hir::HirId,
    ) {
        // Wrap in tables here, not just in visit_nested_body,
        // in order for `check_fn` to be able to use them.
        let old_tables = self.context.tables;
        self.context.tables = self.context.tcx.body_tables(body_id);
        let body = self.context.tcx.hir().body(body_id);
        lint_callback!(self, check_fn, fk, decl, body, span, id);
        hir_visit::walk_fn(self, fk, decl, body_id, span, id);
        lint_callback!(self, check_fn_post, fk, decl, body, span, id);
        self.context.tables = old_tables;
    }

    fn visit_variant_data(
        &mut self,
        s: &'tcx hir::VariantData<'tcx>,
        _: Symbol,
        _: &'tcx hir::Generics<'tcx>,
        _: hir::HirId,
        _: Span,
    ) {
        lint_callback!(self, check_struct_def, s);
        hir_visit::walk_struct_def(self, s);
        lint_callback!(self, check_struct_def_post, s);
    }

    fn visit_struct_field(&mut self, s: &'tcx hir::StructField<'tcx>) {
        self.with_lint_attrs(s.hir_id, &s.attrs, |cx| {
            lint_callback!(cx, check_struct_field, s);
            hir_visit::walk_struct_field(cx, s);
        })
    }

    fn visit_variant(
        &mut self,
        v: &'tcx hir::Variant<'tcx>,
        g: &'tcx hir::Generics<'tcx>,
        item_id: hir::HirId,
    ) {
        self.with_lint_attrs(v.id, &v.attrs, |cx| {
            lint_callback!(cx, check_variant, v);
            hir_visit::walk_variant(cx, v, g, item_id);
            lint_callback!(cx, check_variant_post, v);
        })
    }

    fn visit_ty(&mut self, t: &'tcx hir::Ty<'tcx>) {
        lint_callback!(self, check_ty, t);
        hir_visit::walk_ty(self, t);
    }

    fn visit_name(&mut self, sp: Span, name: Symbol) {
        lint_callback!(self, check_name, sp, name);
    }

    fn visit_mod(&mut self, m: &'tcx hir::Mod<'tcx>, s: Span, n: hir::HirId) {
        if !self.context.only_module {
            self.process_mod(m, s, n);
        }
    }

    fn visit_local(&mut self, l: &'tcx hir::Local<'tcx>) {
        self.with_lint_attrs(l.hir_id, &l.attrs, |cx| {
            lint_callback!(cx, check_local, l);
            hir_visit::walk_local(cx, l);
        })
    }

    fn visit_block(&mut self, b: &'tcx hir::Block<'tcx>) {
        lint_callback!(self, check_block, b);
        hir_visit::walk_block(self, b);
        lint_callback!(self, check_block_post, b);
    }

    fn visit_arm(&mut self, a: &'tcx hir::Arm<'tcx>) {
        lint_callback!(self, check_arm, a);
        hir_visit::walk_arm(self, a);
    }

    fn visit_generic_param(&mut self, p: &'tcx hir::GenericParam<'tcx>) {
        lint_callback!(self, check_generic_param, p);
        hir_visit::walk_generic_param(self, p);
    }

    fn visit_generics(&mut self, g: &'tcx hir::Generics<'tcx>) {
        lint_callback!(self, check_generics, g);
        hir_visit::walk_generics(self, g);
    }

    fn visit_where_predicate(&mut self, p: &'tcx hir::WherePredicate<'tcx>) {
        lint_callback!(self, check_where_predicate, p);
        hir_visit::walk_where_predicate(self, p);
    }

    fn visit_poly_trait_ref(
        &mut self,
        t: &'tcx hir::PolyTraitRef<'tcx>,
        m: hir::TraitBoundModifier,
    ) {
        lint_callback!(self, check_poly_trait_ref, t, m);
        hir_visit::walk_poly_trait_ref(self, t, m);
    }

    fn visit_trait_item(&mut self, trait_item: &'tcx hir::TraitItem<'tcx>) {
        let generics = self.context.generics.take();
        self.context.generics = Some(&trait_item.generics);
        self.with_lint_attrs(trait_item.hir_id, &trait_item.attrs, |cx| {
            cx.with_param_env(trait_item.hir_id, |cx| {
                lint_callback!(cx, check_trait_item, trait_item);
                hir_visit::walk_trait_item(cx, trait_item);
                lint_callback!(cx, check_trait_item_post, trait_item);
            });
        });
        self.context.generics = generics;
    }

    fn visit_impl_item(&mut self, impl_item: &'tcx hir::ImplItem<'tcx>) {
        let generics = self.context.generics.take();
        self.context.generics = Some(&impl_item.generics);
        self.with_lint_attrs(impl_item.hir_id, &impl_item.attrs, |cx| {
            cx.with_param_env(impl_item.hir_id, |cx| {
                lint_callback!(cx, check_impl_item, impl_item);
                hir_visit::walk_impl_item(cx, impl_item);
                lint_callback!(cx, check_impl_item_post, impl_item);
            });
        });
        self.context.generics = generics;
    }

    fn visit_lifetime(&mut self, lt: &'tcx hir::Lifetime) {
        lint_callback!(self, check_lifetime, lt);
        hir_visit::walk_lifetime(self, lt);
    }

    fn visit_path(&mut self, p: &'tcx hir::Path<'tcx>, id: hir::HirId) {
        lint_callback!(self, check_path, p, id);
        hir_visit::walk_path(self, p);
    }

    fn visit_attribute(&mut self, attr: &'tcx ast::Attribute) {
        lint_callback!(self, check_attribute, attr);
    }
}

struct LateLintPassObjects<'a> {
    lints: &'a mut [LateLintPassObject],
}

#[allow(rustc::lint_pass_impl_without_macro)]
impl LintPass for LateLintPassObjects<'_> {
    fn name(&self) -> &'static str {
        panic!()
    }
}

macro_rules! expand_late_lint_pass_impl_methods {
    ([$a:tt, $hir:tt], [$($(#[$attr:meta])* fn $name:ident($($param:ident: $arg:ty),*);)*]) => (
        $(fn $name(&mut self, context: &LateContext<$a, $hir>, $($param: $arg),*) {
            for obj in self.lints.iter_mut() {
                obj.$name(context, $($param),*);
            }
        })*
    )
}

macro_rules! late_lint_pass_impl {
    ([], [$hir:tt], $methods:tt) => (
        impl<'a, $hir> LateLintPass<'a, $hir> for LateLintPassObjects<'_> {
            expand_late_lint_pass_impl_methods!(['a, $hir], $methods);
        }
    )
}

crate::late_lint_methods!(late_lint_pass_impl, [], ['tcx]);

fn late_lint_mod_pass<'tcx, T: for<'a> LateLintPass<'a, 'tcx>>(
    tcx: TyCtxt<'tcx>,
    module_def_id: LocalDefId,
    pass: T,
) {
    let access_levels = &tcx.privacy_access_levels(LOCAL_CRATE);

    let context = LateContext {
        tcx,
        tables: &ty::TypeckTables::empty(None),
        param_env: ty::ParamEnv::empty(),
        access_levels,
        lint_store: unerased_lint_store(tcx),
        last_node_with_lint_attrs: tcx.hir().as_local_hir_id(module_def_id),
        generics: None,
        only_module: true,
    };

    let mut cx = LateContextAndPass { context, pass };

    let (module, span, hir_id) = tcx.hir().get_module(module_def_id);
    cx.process_mod(module, span, hir_id);

    // Visit the crate attributes
    if hir_id == hir::CRATE_HIR_ID {
        walk_list!(cx, visit_attribute, tcx.hir().attrs(hir::CRATE_HIR_ID));
    }
}

pub fn late_lint_mod<'tcx, T: for<'a> LateLintPass<'a, 'tcx>>(
    tcx: TyCtxt<'tcx>,
    module_def_id: LocalDefId,
    builtin_lints: T,
) {
    if tcx.sess.opts.debugging_opts.no_interleave_lints {
        // These passes runs in late_lint_crate with -Z no_interleave_lints
        return;
    }

    late_lint_mod_pass(tcx, module_def_id, builtin_lints);

    let mut passes: Vec<_> =
        unerased_lint_store(tcx).late_module_passes.iter().map(|pass| (pass)()).collect();

    if !passes.is_empty() {
        late_lint_mod_pass(tcx, module_def_id, LateLintPassObjects { lints: &mut passes[..] });
    }
}

fn late_lint_pass_crate<'tcx, T: for<'a> LateLintPass<'a, 'tcx>>(tcx: TyCtxt<'tcx>, pass: T) {
    let access_levels = &tcx.privacy_access_levels(LOCAL_CRATE);

    let krate = tcx.hir().krate();

    let context = LateContext {
        tcx,
        tables: &ty::TypeckTables::empty(None),
        param_env: ty::ParamEnv::empty(),
        access_levels,
        lint_store: unerased_lint_store(tcx),
        last_node_with_lint_attrs: hir::CRATE_HIR_ID,
        generics: None,
        only_module: false,
    };

    let mut cx = LateContextAndPass { context, pass };

    // Visit the whole crate.
    cx.with_lint_attrs(hir::CRATE_HIR_ID, &krate.item.attrs, |cx| {
        // since the root module isn't visited as an item (because it isn't an
        // item), warn for it here.
        lint_callback!(cx, check_crate, krate);

        hir_visit::walk_crate(cx, krate);

        lint_callback!(cx, check_crate_post, krate);
    })
}

fn late_lint_crate<'tcx, T: for<'a> LateLintPass<'a, 'tcx>>(tcx: TyCtxt<'tcx>, builtin_lints: T) {
    let mut passes = unerased_lint_store(tcx).late_passes.iter().map(|p| (p)()).collect::<Vec<_>>();

    if !tcx.sess.opts.debugging_opts.no_interleave_lints {
        if !passes.is_empty() {
            late_lint_pass_crate(tcx, LateLintPassObjects { lints: &mut passes[..] });
        }

        late_lint_pass_crate(tcx, builtin_lints);
    } else {
        for pass in &mut passes {
            tcx.sess.prof.extra_verbose_generic_activity("run_late_lint", pass.name()).run(|| {
                late_lint_pass_crate(tcx, LateLintPassObjects { lints: slice::from_mut(pass) });
            });
        }

        let mut passes: Vec<_> =
            unerased_lint_store(tcx).late_module_passes.iter().map(|pass| (pass)()).collect();

        for pass in &mut passes {
            tcx.sess.prof.extra_verbose_generic_activity("run_late_module_lint", pass.name()).run(
                || {
                    late_lint_pass_crate(tcx, LateLintPassObjects { lints: slice::from_mut(pass) });
                },
            );
        }
    }
}

/// Performs lint checking on a crate.
pub fn check_crate<'tcx, T: for<'a> LateLintPass<'a, 'tcx>>(
    tcx: TyCtxt<'tcx>,
    builtin_lints: impl FnOnce() -> T + Send,
) {
    join(
        || {
            tcx.sess.time("crate_lints", || {
                // Run whole crate non-incremental lints
                late_lint_crate(tcx, builtin_lints());
            });
        },
        || {
            tcx.sess.time("module_lints", || {
                // Run per-module lints
                par_iter(&tcx.hir().krate().modules).for_each(|(&module, _)| {
                    tcx.ensure().lint_mod(tcx.hir().local_def_id(module));
                });
            });
        },
    );
}
