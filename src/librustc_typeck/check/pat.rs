use crate::check::FnCtxt;
use crate::util::nodemap::FxHashMap;
use errors::{Applicability, DiagnosticBuilder, pluralise};
use rustc::hir::{self, PatKind, Pat, HirId};
use rustc::hir::def::{Res, DefKind, CtorKind};
use rustc::hir::pat_util::EnumerateAndAdjustIterator;
use rustc::hir::ptr::P;
use rustc::infer;
use rustc::infer::type_variable::{TypeVariableOrigin, TypeVariableOriginKind};
use rustc::ty::{self, Ty, BindingMode, TypeFoldable};
use rustc::ty::subst::Kind;
use syntax::ast;
use syntax::util::lev_distance::find_best_match_for_name;
use syntax_pos::Span;
use syntax_pos::hygiene::DesugaringKind;

use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::cmp;

use super::report_unexpected_variant_res;

const CANNOT_IMPLICITLY_DEREF_POINTER_TRAIT_OBJ: &str = "\
This error indicates that a pointer to a trait type cannot be implicitly dereferenced by a \
pattern. Every trait defines a type, but because the size of trait implementors isn't fixed, \
this type has no compile-time size. Therefore, all accesses to trait types must be through \
pointers. If you encounter this error you should try to avoid dereferencing the pointer.

You can read more about trait objects in the Trait Objects section of the Reference: \
https://doc.rust-lang.org/reference/types.html#trait-objects";

impl<'a, 'tcx> FnCtxt<'a, 'tcx> {
    pub fn check_pat_top(&self, pat: &'tcx Pat, expected: Ty<'tcx>, discrim_span: Option<Span>) {
        let def_bm = BindingMode::BindByValue(hir::Mutability::MutImmutable);
        self.check_pat(pat, expected, def_bm, discrim_span);
    }

    /// `discrim_span` argument having a `Span` indicates that this pattern is part of a match
    /// expression arm guard, and it points to the match discriminant to add context in type errors.
    /// In the following example, `discrim_span` corresponds to the `a + b` expression:
    ///
    /// ```text
    /// error[E0308]: mismatched types
    ///  --> src/main.rs:5:9
    ///   |
    /// 4 |    let temp: usize = match a + b {
    ///   |                            ----- this expression has type `usize`
    /// 5 |         Ok(num) => num,
    ///   |         ^^^^^^^ expected usize, found enum `std::result::Result`
    ///   |
    ///   = note: expected type `usize`
    ///              found type `std::result::Result<_, _>`
    /// ```
    fn check_pat(
        &self,
        pat: &'tcx Pat,
        expected: Ty<'tcx>,
        def_bm: BindingMode,
        discrim_span: Option<Span>,
    ) {
        debug!("check_pat(pat={:?},expected={:?},def_bm={:?})", pat, expected, def_bm);

        let path_resolution = match &pat.node {
            PatKind::Path(qpath) => Some(self.resolve_ty_and_res_ufcs(qpath, pat.hir_id, pat.span)),
            _ => None,
        };
        let is_nrp = self.is_non_ref_pat(pat, path_resolution.map(|(res, ..)| res));
        let (expected, def_bm) = self.calc_default_binding_mode(pat, expected, def_bm, is_nrp);

        let ty = match &pat.node {
            PatKind::Wild => expected,
            PatKind::Lit(lt) => self.check_pat_lit(pat.span, lt, expected, discrim_span),
            PatKind::Range(begin, end, _) => {
                match self.check_pat_range(pat.span, begin, end, expected, discrim_span) {
                    None => return,
                    Some(ty) => ty,
                }
            }
            PatKind::Binding(ba, var_id, _, sub) => {
                let sub = sub.as_deref();
                self.check_pat_ident(pat, *ba, *var_id, sub, expected, def_bm, discrim_span)
            }
            PatKind::TupleStruct(qpath, subpats, ddpos) => {
                self.check_pat_tuple_struct(
                    pat,
                    qpath,
                    subpats,
                    *ddpos,
                    expected,
                    def_bm,
                    discrim_span,
                )
            }
            PatKind::Path(qpath) => {
                self.check_pat_path(pat, path_resolution.unwrap(), qpath, expected)
            }
            PatKind::Struct(qpath, fields, etc) => {
                self.check_pat_struct(pat, qpath, fields, *etc, expected, def_bm, discrim_span)
            }
            PatKind::Or(pats) => {
                let expected_ty = self.structurally_resolved_type(pat.span, expected);
                for pat in pats {
                    self.check_pat(pat, expected, def_bm, discrim_span);
                }
                expected_ty
            }
            PatKind::Tuple(elements, ddpos) => {
                self.check_pat_tuple(pat.span, elements, *ddpos, expected, def_bm, discrim_span)
            }
            PatKind::Box(inner) => {
                self.check_pat_box(pat.span, inner, expected, def_bm, discrim_span)
            }
            PatKind::Ref(inner, mutbl) => {
                self.check_pat_ref(pat, inner, *mutbl, expected, def_bm, discrim_span)
            }
            PatKind::Slice(before, slice, after) => {
                let slice = slice.as_deref();
                self.check_pat_slice(pat.span, before, slice, after, expected, def_bm, discrim_span)
            }
        };

        self.write_ty(pat.hir_id, ty);

        // (note_1): In most of the cases where (note_1) is referenced
        // (literals and constants being the exception), we relate types
        // using strict equality, even though subtyping would be sufficient.
        // There are a few reasons for this, some of which are fairly subtle
        // and which cost me (nmatsakis) an hour or two debugging to remember,
        // so I thought I'd write them down this time.
        //
        // 1. There is no loss of expressiveness here, though it does
        // cause some inconvenience. What we are saying is that the type
        // of `x` becomes *exactly* what is expected. This can cause unnecessary
        // errors in some cases, such as this one:
        //
        // ```
        // fn foo<'x>(x: &'x int) {
        //    let a = 1;
        //    let mut z = x;
        //    z = &a;
        // }
        // ```
        //
        // The reason we might get an error is that `z` might be
        // assigned a type like `&'x int`, and then we would have
        // a problem when we try to assign `&a` to `z`, because
        // the lifetime of `&a` (i.e., the enclosing block) is
        // shorter than `'x`.
        //
        // HOWEVER, this code works fine. The reason is that the
        // expected type here is whatever type the user wrote, not
        // the initializer's type. In this case the user wrote
        // nothing, so we are going to create a type variable `Z`.
        // Then we will assign the type of the initializer (`&'x
        // int`) as a subtype of `Z`: `&'x int <: Z`. And hence we
        // will instantiate `Z` as a type `&'0 int` where `'0` is
        // a fresh region variable, with the constraint that `'x :
        // '0`.  So basically we're all set.
        //
        // Note that there are two tests to check that this remains true
        // (`regions-reassign-{match,let}-bound-pointer.rs`).
        //
        // 2. Things go horribly wrong if we use subtype. The reason for
        // THIS is a fairly subtle case involving bound regions. See the
        // `givens` field in `region_constraints`, as well as the test
        // `regions-relate-bound-regions-on-closures-to-inference-variables.rs`,
        // for details. Short version is that we must sometimes detect
        // relationships between specific region variables and regions
        // bound in a closure signature, and that detection gets thrown
        // off when we substitute fresh region variables here to enable
        // subtyping.
    }

    /// Compute the new expected type and default binding mode from the old ones
    /// as well as the pattern form we are currently checking.
    fn calc_default_binding_mode(
        &self,
        pat: &'tcx Pat,
        expected: Ty<'tcx>,
        def_bm: BindingMode,
        is_non_ref_pat: bool,
    ) -> (Ty<'tcx>, BindingMode) {
        if is_non_ref_pat {
            debug!("pattern is non reference pattern");
            self.peel_off_references(pat, expected, def_bm)
        } else {
            // When you encounter a `&pat` pattern, reset to "by
            // value". This is so that `x` and `y` here are by value,
            // as they appear to be:
            //
            // ```
            // match &(&22, &44) {
            //   (&x, &y) => ...
            // }
            // ```
            //
            // See issue #46688.
            let def_bm = match pat.node {
                PatKind::Ref(..) => ty::BindByValue(hir::MutImmutable),
                _ => def_bm,
            };
            (expected, def_bm)
        }
    }

    /// Is the pattern a "non reference pattern"?
    /// When the pattern is a path pattern, `opt_path_res` must be `Some(res)`.
    fn is_non_ref_pat(&self, pat: &'tcx Pat, opt_path_res: Option<Res>) -> bool {
        match pat.node {
            PatKind::Struct(..) |
            PatKind::TupleStruct(..) |
            PatKind::Or(_) |
            PatKind::Tuple(..) |
            PatKind::Box(_) |
            PatKind::Range(..) |
            PatKind::Slice(..) => true,
            PatKind::Lit(ref lt) => {
                let ty = self.check_expr(lt);
                match ty.sty {
                    ty::Ref(..) => false,
                    _ => true,
                }
            }
            PatKind::Path(_) => {
                match opt_path_res.unwrap() {
                    Res::Def(DefKind::Const, _) | Res::Def(DefKind::AssocConst, _) => false,
                    _ => true,
                }
            }
            PatKind::Wild |
            PatKind::Binding(..) |
            PatKind::Ref(..) => false,
        }
    }

    /// Peel off as many immediately nested `& mut?` from the expected type as possible
    /// and return the new expected type and binding default binding mode.
    /// The adjustments vector, if non-empty is stored in a table.
    fn peel_off_references(
        &self,
        pat: &'tcx Pat,
        expected: Ty<'tcx>,
        mut def_bm: BindingMode,
    ) -> (Ty<'tcx>, BindingMode) {
        let mut expected = self.resolve_type_vars_with_obligations(&expected);

        // Peel off as many `&` or `&mut` from the scrutinee type as possible. For example,
        // for `match &&&mut Some(5)` the loop runs three times, aborting when it reaches
        // the `Some(5)` which is not of type Ref.
        //
        // For each ampersand peeled off, update the binding mode and push the original
        // type into the adjustments vector.
        //
        // See the examples in `ui/match-defbm*.rs`.
        let mut pat_adjustments = vec![];
        while let ty::Ref(_, inner_ty, inner_mutability) = expected.sty {
            debug!("inspecting {:?}", expected);

            debug!("current discriminant is Ref, inserting implicit deref");
            // Preserve the reference type. We'll need it later during HAIR lowering.
            pat_adjustments.push(expected);

            expected = inner_ty;
            def_bm = ty::BindByReference(match def_bm {
                // If default binding mode is by value, make it `ref` or `ref mut`
                // (depending on whether we observe `&` or `&mut`).
                ty::BindByValue(_) |
                // When `ref mut`, stay a `ref mut` (on `&mut`) or downgrade to `ref` (on `&`).
                ty::BindByReference(hir::Mutability::MutMutable) => inner_mutability,
                // Once a `ref`, always a `ref`.
                // This is because a `& &mut` cannot mutate the underlying value.
                ty::BindByReference(m @ hir::Mutability::MutImmutable) => m,
            });
        }

        if pat_adjustments.len() > 0 {
            debug!("default binding mode is now {:?}", def_bm);
            self.inh.tables.borrow_mut()
                .pat_adjustments_mut()
                .insert(pat.hir_id, pat_adjustments);
        }

        (expected, def_bm)
    }

    fn check_pat_lit(
        &self,
        span: Span,
        lt: &hir::Expr,
        expected: Ty<'tcx>,
        discrim_span: Option<Span>,
    ) -> Ty<'tcx> {
        // We've already computed the type above (when checking for a non-ref pat),
        // so avoid computing it again.
        let ty = self.node_ty(lt.hir_id);

        // Byte string patterns behave the same way as array patterns
        // They can denote both statically and dynamically-sized byte arrays.
        let mut pat_ty = ty;
        if let hir::ExprKind::Lit(ref lt) = lt.node {
            if let ast::LitKind::ByteStr(_) = lt.node {
                let expected_ty = self.structurally_resolved_type(span, expected);
                if let ty::Ref(_, r_ty, _) = expected_ty.sty {
                    if let ty::Slice(_) = r_ty.sty {
                        let tcx = self.tcx;
                        pat_ty = tcx.mk_imm_ref(
                            tcx.lifetimes.re_static,
                            tcx.mk_slice(tcx.types.u8),
                        );
                    }
                }
            }
        }

        // Somewhat surprising: in this case, the subtyping relation goes the
        // opposite way as the other cases. Actually what we really want is not
        // a subtyping relation at all but rather that there exists a LUB
        // (so that they can be compared). However, in practice, constants are
        // always scalars or strings. For scalars subtyping is irrelevant,
        // and for strings `ty` is type is `&'static str`, so if we say that
        //
        //     &'static str <: expected
        //
        // then that's equivalent to there existing a LUB.
        if let Some(mut err) = self.demand_suptype_diag(span, expected, pat_ty) {
            err.emit_unless(discrim_span
                .filter(|&s| {
                    // In the case of `if`- and `while`-expressions we've already checked
                    // that `scrutinee: bool`. We know that the pattern is `true`,
                    // so an error here would be a duplicate and from the wrong POV.
                    s.is_desugaring(DesugaringKind::CondTemporary)
                })
                .is_some());
        }

        pat_ty
    }

    fn check_pat_range(
        &self,
        span: Span,
        begin: &'tcx hir::Expr,
        end: &'tcx hir::Expr,
        expected: Ty<'tcx>,
        discrim_span: Option<Span>,
    ) -> Option<Ty<'tcx>> {
        let lhs_ty = self.check_expr(begin);
        let rhs_ty = self.check_expr(end);

        // Check that both end-points are of numeric or char type.
        let numeric_or_char = |ty: Ty<'_>| {
            ty.is_numeric()
            || ty.is_char()
            || ty.references_error()
        };
        let lhs_compat = numeric_or_char(lhs_ty);
        let rhs_compat = numeric_or_char(rhs_ty);

        if !lhs_compat || !rhs_compat {
            let span = if !lhs_compat && !rhs_compat {
                span
            } else if !lhs_compat {
                begin.span
            } else {
                end.span
            };

            let mut err = struct_span_err!(
                self.tcx.sess,
                span,
                E0029,
                "only char and numeric types are allowed in range patterns"
            );
            err.span_label(span, "ranges require char or numeric types");
            err.note(&format!("start type: {}", self.ty_to_string(lhs_ty)));
            err.note(&format!("end type: {}", self.ty_to_string(rhs_ty)));
            if self.tcx.sess.teach(&err.get_code().unwrap()) {
                err.note(
                    "In a match expression, only numbers and characters can be matched \
                        against a range. This is because the compiler checks that the range \
                        is non-empty at compile-time, and is unable to evaluate arbitrary \
                        comparison functions. If you want to capture values of an orderable \
                        type between two end-points, you can use a guard."
                    );
            }
            err.emit();
            return None;
        }

        // Now that we know the types can be unified we find the unified type and use
        // it to type the entire expression.
        let common_type = self.resolve_vars_if_possible(&lhs_ty);

        // Subtyping doesn't matter here, as the value is some kind of scalar.
        self.demand_eqtype_pat(span, expected, lhs_ty, discrim_span);
        self.demand_eqtype_pat(span, expected, rhs_ty, discrim_span);
        Some(common_type)
    }

    fn check_pat_ident(
        &self,
        pat: &Pat,
        ba: hir::BindingAnnotation,
        var_id: HirId,
        sub: Option<&'tcx Pat>,
        expected: Ty<'tcx>,
        def_bm: BindingMode,
        discrim_span: Option<Span>,
    ) -> Ty<'tcx> {
        // Determine the binding mode...
        let bm = match ba {
            hir::BindingAnnotation::Unannotated => def_bm,
            _ => BindingMode::convert(ba),
        };
        // ...and store it in a side table:
        self.inh
            .tables
            .borrow_mut()
            .pat_binding_modes_mut()
            .insert(pat.hir_id, bm);

        debug!("check_pat_ident: pat.hir_id={:?} bm={:?}", pat.hir_id, bm);

        let local_ty = self.local_ty(pat.span, pat.hir_id).decl_ty;
        let eq_ty = match bm {
            ty::BindByReference(mutbl) => {
                // If the binding is like `ref x | ref const x | ref mut x`
                // then `x` is assigned a value of type `&M T` where M is the
                // mutability and T is the expected type.
                let region_ty = self.new_ref_ty(pat.span, mutbl, expected);

                // `x` is assigned a value of type `&M T`, hence `&M T <: typeof(x)`
                // is required. However, we use equality, which is stronger.
                // See (note_1) for an explanation.
                region_ty
            }
            // Otherwise, the type of x is the expected type `T`.
            ty::BindByValue(_) => {
                // As above, `T <: typeof(x)` is required, but we use equality, see (note_1).
                expected
            }
        };
        self.demand_eqtype_pat(pat.span, eq_ty, local_ty, discrim_span);

        // If there are multiple arms, make sure they all agree on
        // what the type of the binding `x` ought to be.
        if var_id != pat.hir_id {
            let vt = self.local_ty(pat.span, var_id).decl_ty;
            self.demand_eqtype_pat(pat.span, vt, local_ty, discrim_span);
        }

        if let Some(p) = sub {
            self.check_pat(&p, expected, def_bm, discrim_span);
        }

        local_ty
    }

    fn borrow_pat_suggestion(
        &self,
        err: &mut DiagnosticBuilder<'_>,
        pat: &Pat,
        inner: &Pat,
        expected: Ty<'tcx>,
    ) {
        let tcx = self.tcx;
        if let PatKind::Binding(..) = inner.node {
            let binding_parent_id = tcx.hir().get_parent_node(pat.hir_id);
            let binding_parent = tcx.hir().get(binding_parent_id);
            debug!("inner {:?} pat {:?} parent {:?}", inner, pat, binding_parent);
            match binding_parent {
                hir::Node::Param(hir::Param { span, .. }) => {
                    if let Ok(snippet) = tcx.sess.source_map().span_to_snippet(inner.span) {
                        err.span_suggestion(
                            *span,
                            &format!("did you mean `{}`", snippet),
                            format!(" &{}", expected),
                            Applicability::MachineApplicable,
                        );
                    }
                }
                hir::Node::Arm(_) |
                hir::Node::Pat(_) => {
                    // rely on match ergonomics or it might be nested `&&pat`
                    if let Ok(snippet) = tcx.sess.source_map().span_to_snippet(inner.span) {
                        err.span_suggestion(
                            pat.span,
                            "you can probably remove the explicit borrow",
                            snippet,
                            Applicability::MaybeIncorrect,
                        );
                    }
                }
                _ => {} // don't provide suggestions in other cases #55175
            }
        }
    }

    pub fn check_dereferencable(&self, span: Span, expected: Ty<'tcx>, inner: &Pat) -> bool {
        if let PatKind::Binding(..) = inner.node {
            if let Some(mt) = self.shallow_resolve(expected).builtin_deref(true) {
                if let ty::Dynamic(..) = mt.ty.sty {
                    // This is "x = SomeTrait" being reduced from
                    // "let &x = &SomeTrait" or "let box x = Box<SomeTrait>", an error.
                    let type_str = self.ty_to_string(expected);
                    let mut err = struct_span_err!(
                        self.tcx.sess,
                        span,
                        E0033,
                        "type `{}` cannot be dereferenced",
                        type_str
                    );
                    err.span_label(span, format!("type `{}` cannot be dereferenced", type_str));
                    if self.tcx.sess.teach(&err.get_code().unwrap()) {
                        err.note(CANNOT_IMPLICITLY_DEREF_POINTER_TRAIT_OBJ);
                    }
                    err.emit();
                    return false
                }
            }
        }
        true
    }

    fn check_pat_struct(
        &self,
        pat: &'tcx Pat,
        qpath: &hir::QPath,
        fields: &'tcx [hir::FieldPat],
        etc: bool,
        expected: Ty<'tcx>,
        def_bm: BindingMode,
        discrim_span: Option<Span>,
    ) -> Ty<'tcx> {
        // Resolve the path and check the definition for errors.
        let (variant, pat_ty) = if let Some(variant_ty) = self.check_struct_path(qpath, pat.hir_id)
        {
            variant_ty
        } else {
            for field in fields {
                self.check_pat(&field.pat, self.tcx.types.err, def_bm, discrim_span);
            }
            return self.tcx.types.err;
        };

        // Type-check the path.
        self.demand_eqtype_pat(pat.span, expected, pat_ty, discrim_span);

        // Type-check subpatterns.
        if self.check_struct_pat_fields(pat_ty, pat.hir_id, pat.span, variant, fields, etc, def_bm)
        {
            pat_ty
        } else {
            self.tcx.types.err
        }
    }

    fn check_pat_path(
        &self,
        pat: &Pat,
        path_resolution: (Res, Option<Ty<'tcx>>, &'b [hir::PathSegment]),
        qpath: &hir::QPath,
        expected: Ty<'tcx>,
    ) -> Ty<'tcx> {
        let tcx = self.tcx;

        // We have already resolved the path.
        let (res, opt_ty, segments) = path_resolution;
        match res {
            Res::Err => {
                self.set_tainted_by_errors();
                return tcx.types.err;
            }
            Res::Def(DefKind::Method, _) |
            Res::Def(DefKind::Ctor(_, CtorKind::Fictive), _) |
            Res::Def(DefKind::Ctor(_, CtorKind::Fn), _) => {
                report_unexpected_variant_res(tcx, res, pat.span, qpath);
                return tcx.types.err;
            }
            Res::Def(DefKind::Ctor(_, CtorKind::Const), _) | Res::SelfCtor(..) |
            Res::Def(DefKind::Const, _) | Res::Def(DefKind::AssocConst, _) => {} // OK
            _ => bug!("unexpected pattern resolution: {:?}", res)
        }

        // Type-check the path.
        let pat_ty = self.instantiate_value_path(segments, opt_ty, res, pat.span, pat.hir_id).0;
        self.demand_suptype(pat.span, expected, pat_ty);
        pat_ty
    }

    fn check_pat_tuple_struct(
        &self,
        pat: &Pat,
        qpath: &hir::QPath,
        subpats: &'tcx [P<Pat>],
        ddpos: Option<usize>,
        expected: Ty<'tcx>,
        def_bm: BindingMode,
        match_arm_pat_span: Option<Span>,
    ) -> Ty<'tcx> {
        let tcx = self.tcx;
        let on_error = || {
            for pat in subpats {
                self.check_pat(&pat, tcx.types.err, def_bm, match_arm_pat_span);
            }
        };
        let report_unexpected_res = |res: Res| {
            let msg = format!("expected tuple struct/variant, found {} `{}`",
                              res.descr(),
                              hir::print::to_string(tcx.hir(), |s| s.print_qpath(qpath, false)));
            let mut err = struct_span_err!(tcx.sess, pat.span, E0164, "{}", msg);
            match (res, &pat.node) {
                (Res::Def(DefKind::Fn, _), _) | (Res::Def(DefKind::Method, _), _) => {
                    err.span_label(pat.span, "`fn` calls are not allowed in patterns");
                    err.help("for more information, visit \
                              https://doc.rust-lang.org/book/ch18-00-patterns.html");
                }
                _ => {
                    err.span_label(pat.span, "not a tuple variant or struct");
                }
            }
            err.emit();
            on_error();
        };

        // Resolve the path and check the definition for errors.
        let (res, opt_ty, segments) = self.resolve_ty_and_res_ufcs(qpath, pat.hir_id, pat.span);
        if res == Res::Err {
            self.set_tainted_by_errors();
            on_error();
            return self.tcx.types.err;
        }

        // Type-check the path.
        let (pat_ty, res) = self.instantiate_value_path(segments, opt_ty, res, pat.span,
            pat.hir_id);
        if !pat_ty.is_fn() {
            report_unexpected_res(res);
            return tcx.types.err;
        }

        let variant = match res {
            Res::Err => {
                self.set_tainted_by_errors();
                on_error();
                return tcx.types.err;
            }
            Res::Def(DefKind::AssocConst, _) | Res::Def(DefKind::Method, _) => {
                report_unexpected_res(res);
                return tcx.types.err;
            }
            Res::Def(DefKind::Ctor(_, CtorKind::Fn), _) => {
                tcx.expect_variant_res(res)
            }
            _ => bug!("unexpected pattern resolution: {:?}", res)
        };

        // Replace constructor type with constructed type for tuple struct patterns.
        let pat_ty = pat_ty.fn_sig(tcx).output();
        let pat_ty = pat_ty.no_bound_vars().expect("expected fn type");

        self.demand_eqtype_pat(pat.span, expected, pat_ty, match_arm_pat_span);

        // Type-check subpatterns.
        if subpats.len() == variant.fields.len()
            || subpats.len() < variant.fields.len() && ddpos.is_some()
        {
            let substs = match pat_ty.sty {
                ty::Adt(_, substs) => substs,
                _ => bug!("unexpected pattern type {:?}", pat_ty),
            };
            for (i, subpat) in subpats.iter().enumerate_and_adjust(variant.fields.len(), ddpos) {
                let field_ty = self.field_ty(subpat.span, &variant.fields[i], substs);
                self.check_pat(&subpat, field_ty, def_bm, match_arm_pat_span);

                self.tcx.check_stability(variant.fields[i].did, Some(pat.hir_id), subpat.span);
            }
        } else {
            // Pattern has wrong number of fields.
            self.e0023(pat.span, res, &subpats, &variant.fields, expected);
            on_error();
            return tcx.types.err;
        }
        pat_ty
    }

    fn e0023(
        &self,
        pat_span: Span,
        res: Res,
        subpats: &'tcx [P<Pat>],
        fields: &[ty::FieldDef],
        expected: Ty<'tcx>
    ) {
        let subpats_ending = pluralise!(subpats.len());
        let fields_ending = pluralise!(fields.len());
        let missing_parenthesis = match expected.sty {
            ty::Adt(_, substs) if fields.len() == 1 => {
                let field_ty = fields[0].ty(self.tcx, substs);
                match field_ty.sty {
                    ty::Tuple(_) => field_ty.tuple_fields().count() == subpats.len(),
                    _ => false,
                }
            }
            _ => false,
        };
        let res_span = self.tcx.def_span(res.def_id());
        let mut err = struct_span_err!(
            self.tcx.sess,
            pat_span,
            E0023,
            "this pattern has {} field{}, but the corresponding {} has {} field{}",
            subpats.len(),
            subpats_ending,
            res.descr(),
            fields.len(),
            fields_ending,
        );
        err.span_label(pat_span, format!(
                "expected {} field{}, found {}",
                fields.len(),
                fields_ending,
                subpats.len(),
            ))
            .span_label(res_span, format!("{} defined here", res.descr()));

        if missing_parenthesis {
            err.multipart_suggestion(
                "missing parenthesis",
                vec![(subpats[0].span.shrink_to_lo(), "(".to_string()),
                    (subpats[subpats.len()-1].span.shrink_to_hi(), ")".to_string())],
                Applicability::MachineApplicable,
            );
        }

        err.emit();
    }

    fn check_pat_tuple(
        &self,
        span: Span,
        elements: &'tcx [P<Pat>],
        ddpos: Option<usize>,
        expected: Ty<'tcx>,
        def_bm: BindingMode,
        discrim_span: Option<Span>,
    ) -> Ty<'tcx> {
        let tcx = self.tcx;
        let mut expected_len = elements.len();
        if ddpos.is_some() {
            // Require known type only when `..` is present.
            if let ty::Tuple(ref tys) = self.structurally_resolved_type(span, expected).sty {
                expected_len = tys.len();
            }
        }
        let max_len = cmp::max(expected_len, elements.len());

        let element_tys_iter = (0..max_len).map(|_| {
            Kind::from(self.next_ty_var(
                // FIXME: `MiscVariable` for now -- obtaining the span and name information
                // from all tuple elements isn't trivial.
                TypeVariableOrigin {
                    kind: TypeVariableOriginKind::TypeInference,
                    span,
                },
            ))
        });
        let element_tys = tcx.mk_substs(element_tys_iter);
        let pat_ty = tcx.mk_ty(ty::Tuple(element_tys));
        if let Some(mut err) = self.demand_eqtype_diag(span, expected, pat_ty) {
            err.emit();
            // Walk subpatterns with an expected type of `err` in this case to silence
            // further errors being emitted when using the bindings. #50333
            let element_tys_iter = (0..max_len).map(|_| tcx.types.err);
            for (_, elem) in elements.iter().enumerate_and_adjust(max_len, ddpos) {
                self.check_pat(elem, &tcx.types.err, def_bm, discrim_span);
            }
            tcx.mk_tup(element_tys_iter)
        } else {
            for (i, elem) in elements.iter().enumerate_and_adjust(max_len, ddpos) {
                self.check_pat(elem, &element_tys[i].expect_ty(), def_bm, discrim_span);
            }
            pat_ty
        }
    }

    fn check_struct_pat_fields(
        &self,
        adt_ty: Ty<'tcx>,
        pat_id: HirId,
        span: Span,
        variant: &'tcx ty::VariantDef,
        fields: &'tcx [hir::FieldPat],
        etc: bool,
        def_bm: BindingMode,
    ) -> bool {
        let tcx = self.tcx;

        let (substs, adt) = match adt_ty.sty {
            ty::Adt(adt, substs) => (substs, adt),
            _ => span_bug!(span, "struct pattern is not an ADT")
        };
        let kind_name = adt.variant_descr();

        // Index the struct fields' types.
        let field_map = variant.fields
            .iter()
            .enumerate()
            .map(|(i, field)| (field.ident.modern(), (i, field)))
            .collect::<FxHashMap<_, _>>();

        // Keep track of which fields have already appeared in the pattern.
        let mut used_fields = FxHashMap::default();
        let mut no_field_errors = true;

        let mut inexistent_fields = vec![];
        // Typecheck each field.
        for field in fields {
            let span = field.span;
            let ident = tcx.adjust_ident(field.ident, variant.def_id);
            let field_ty = match used_fields.entry(ident) {
                Occupied(occupied) => {
                    self.error_field_already_bound(span, field.ident, *occupied.get());
                    no_field_errors = false;
                    tcx.types.err
                }
                Vacant(vacant) => {
                    vacant.insert(span);
                    field_map.get(&ident)
                        .map(|(i, f)| {
                            self.write_field_index(field.hir_id, *i);
                            self.tcx.check_stability(f.did, Some(pat_id), span);
                            self.field_ty(span, f, substs)
                        })
                        .unwrap_or_else(|| {
                            inexistent_fields.push(field.ident);
                            no_field_errors = false;
                            tcx.types.err
                        })
                }
            };

            self.check_pat(&field.pat, field_ty, def_bm, None);
        }

        let mut unmentioned_fields = variant.fields
                .iter()
                .map(|field| field.ident.modern())
                .filter(|ident| !used_fields.contains_key(&ident))
                .collect::<Vec<_>>();

        if inexistent_fields.len() > 0 && !variant.recovered {
            self.error_inexistent_fields(
                kind_name,
                &inexistent_fields,
                &mut unmentioned_fields,
                variant
            );
        }

        // Require `..` if struct has non_exhaustive attribute.
        if variant.is_field_list_non_exhaustive() && !adt.did.is_local() && !etc {
            span_err!(tcx.sess, span, E0638,
                      "`..` required with {} marked as non-exhaustive",
                      kind_name);
        }

        // Report an error if incorrect number of the fields were specified.
        if kind_name == "union" {
            if fields.len() != 1 {
                tcx.sess.span_err(span, "union patterns should have exactly one field");
            }
            if etc {
                tcx.sess.span_err(span, "`..` cannot be used in union patterns");
            }
        } else if !etc && unmentioned_fields.len() > 0 {
            self.error_unmentioned_fields(span, &unmentioned_fields, variant);
        }
        no_field_errors
    }

    fn error_field_already_bound(&self, span: Span, ident: ast::Ident, other_field: Span) {
        struct_span_err!(
            self.tcx.sess, span, E0025,
            "field `{}` bound multiple times in the pattern",
            ident
        )
        .span_label(span, format!("multiple uses of `{}` in pattern", ident))
        .span_label(other_field, format!("first use of `{}`", ident))
        .emit();
    }

    fn error_inexistent_fields(
        &self,
        kind_name: &str,
        inexistent_fields: &[ast::Ident],
        unmentioned_fields: &mut Vec<ast::Ident>,
        variant: &ty::VariantDef,
    ) {
        let tcx = self.tcx;
        let (field_names, t, plural) = if inexistent_fields.len() == 1 {
            (format!("a field named `{}`", inexistent_fields[0]), "this", "")
        } else {
            (format!("fields named {}",
                        inexistent_fields.iter()
                        .map(|ident| format!("`{}`", ident))
                        .collect::<Vec<String>>()
                        .join(", ")), "these", "s")
        };
        let spans = inexistent_fields.iter().map(|ident| ident.span).collect::<Vec<_>>();
        let mut err = struct_span_err!(tcx.sess,
                                        spans,
                                        E0026,
                                        "{} `{}` does not have {}",
                                        kind_name,
                                        tcx.def_path_str(variant.def_id),
                                        field_names);
        if let Some(ident) = inexistent_fields.last() {
            err.span_label(ident.span,
                            format!("{} `{}` does not have {} field{}",
                                    kind_name,
                                    tcx.def_path_str(variant.def_id),
                                    t,
                                    plural));
            if plural == "" {
                let input = unmentioned_fields.iter().map(|field| &field.name);
                let suggested_name =
                    find_best_match_for_name(input, &ident.as_str(), None);
                if let Some(suggested_name) = suggested_name {
                    err.span_suggestion(
                        ident.span,
                        "a field with a similar name exists",
                        suggested_name.to_string(),
                        Applicability::MaybeIncorrect,
                    );

                    // we don't want to throw `E0027` in case we have thrown `E0026` for them
                    unmentioned_fields.retain(|&x| x.as_str() != suggested_name.as_str());
                }
            }
        }
        if tcx.sess.teach(&err.get_code().unwrap()) {
            err.note(
                "This error indicates that a struct pattern attempted to \
                    extract a non-existent field from a struct. Struct fields \
                    are identified by the name used before the colon : so struct \
                    patterns should resemble the declaration of the struct type \
                    being matched.\n\n\
                    If you are using shorthand field patterns but want to refer \
                    to the struct field by a different name, you should rename \
                    it explicitly."
            );
        }
        err.emit();
    }

    fn error_unmentioned_fields(
        &self,
        span: Span,
        unmentioned_fields: &[ast::Ident],
        variant: &ty::VariantDef,
    ) {
        let field_names = if unmentioned_fields.len() == 1 {
            format!("field `{}`", unmentioned_fields[0])
        } else {
            let fields = unmentioned_fields.iter()
                .map(|name| format!("`{}`", name))
                .collect::<Vec<String>>()
                .join(", ");
            format!("fields {}", fields)
        };
        let mut diag = struct_span_err!(
            self.tcx.sess, span, E0027,
            "pattern does not mention {}",
            field_names
        );
        diag.span_label(span, format!("missing {}", field_names));
        if variant.ctor_kind == CtorKind::Fn {
            diag.note("trying to match a tuple variant with a struct variant pattern");
        }
        if self.tcx.sess.teach(&diag.get_code().unwrap()) {
            diag.note(
                "This error indicates that a pattern for a struct fails to specify a \
                    sub-pattern for every one of the struct's fields. Ensure that each field \
                    from the struct's definition is mentioned in the pattern, or use `..` to \
                    ignore unwanted fields."
            );
        }
        diag.emit();
    }

    fn check_pat_box(
        &self,
        span: Span,
        inner: &'tcx Pat,
        expected: Ty<'tcx>,
        def_bm: BindingMode,
        discrim_span: Option<Span>,
    ) -> Ty<'tcx> {
        let tcx = self.tcx;
        let (box_ty, inner_ty) = if self.check_dereferencable(span, expected, &inner) {
            // Here, `demand::subtype` is good enough, but I don't
            // think any errors can be introduced by using `demand::eqtype`.
            let inner_ty = self.next_ty_var(TypeVariableOrigin {
                kind: TypeVariableOriginKind::TypeInference,
                span: inner.span,
            });
            let box_ty = tcx.mk_box(inner_ty);
            self.demand_eqtype_pat(span, expected, box_ty, discrim_span);
            (box_ty, inner_ty)
        } else {
            (tcx.types.err, tcx.types.err)
        };
        self.check_pat(&inner, inner_ty, def_bm, discrim_span);
        box_ty
    }

    fn check_pat_ref(
        &self,
        pat: &Pat,
        inner: &'tcx Pat,
        mutbl: hir::Mutability,
        expected: Ty<'tcx>,
        def_bm: BindingMode,
        discrim_span: Option<Span>,
    ) -> Ty<'tcx> {
        let tcx = self.tcx;
        let expected = self.shallow_resolve(expected);
        let (rptr_ty, inner_ty) = if self.check_dereferencable(pat.span, expected, &inner) {
            // `demand::subtype` would be good enough, but using `eqtype` turns
            // out to be equally general. See (note_1) for details.

            // Take region, inner-type from expected type if we can,
            // to avoid creating needless variables. This also helps with
            // the bad  interactions of the given hack detailed in (note_1).
            debug!("check_pat_ref: expected={:?}", expected);
            match expected.sty {
                ty::Ref(_, r_ty, r_mutbl) if r_mutbl == mutbl => (expected, r_ty),
                _ => {
                    let inner_ty = self.next_ty_var(
                        TypeVariableOrigin {
                            kind: TypeVariableOriginKind::TypeInference,
                            span: inner.span,
                        }
                    );
                    let rptr_ty = self.new_ref_ty(pat.span, mutbl, inner_ty);
                    debug!("check_pat_ref: demanding {:?} = {:?}", expected, rptr_ty);
                    let err = self.demand_eqtype_diag(pat.span, expected, rptr_ty);

                    // Look for a case like `fn foo(&foo: u32)` and suggest
                    // `fn foo(foo: &u32)`
                    if let Some(mut err) = err {
                        self.borrow_pat_suggestion(&mut err, &pat, &inner, &expected);
                        err.emit();
                    }
                    (rptr_ty, inner_ty)
                }
            }
        } else {
            (tcx.types.err, tcx.types.err)
        };
        self.check_pat(&inner, inner_ty, def_bm, discrim_span);
        rptr_ty
    }

    /// Create a reference type with a fresh region variable.
    fn new_ref_ty(&self, span: Span, mutbl: hir::Mutability, ty: Ty<'tcx>) -> Ty<'tcx> {
        let region = self.next_region_var(infer::PatternRegion(span));
        let mt = ty::TypeAndMut { ty, mutbl };
        self.tcx.mk_ref(region, mt)
    }

    fn check_pat_slice(
        &self,
        span: Span,
        before: &'tcx [P<Pat>],
        slice: Option<&'tcx Pat>,
        after: &'tcx [P<Pat>],
        expected: Ty<'tcx>,
        def_bm: BindingMode,
        discrim_span: Option<Span>,
    ) -> Ty<'tcx> {
        let tcx = self.tcx;
        let expected_ty = self.structurally_resolved_type(span, expected);
        let (inner_ty, slice_ty) = match expected_ty.sty {
            ty::Array(inner_ty, size) => {
                let slice_ty = if let Some(size) = size.try_eval_usize(tcx, self.param_env) {
                    let min_len = before.len() as u64 + after.len() as u64;
                    if slice.is_none() {
                        if min_len != size {
                            self.error_scrutinee_inconsistent_length(span, min_len, size)
                        }
                        tcx.types.err
                    } else if let Some(rest) = size.checked_sub(min_len) {
                        tcx.mk_array(inner_ty, rest)
                    } else {
                        self.error_scrutinee_with_rest_inconsistent_length(span, min_len, size);
                        tcx.types.err
                    }
                } else {
                    self.error_scrutinee_unfixed_length(span);
                    tcx.types.err
                };
                (inner_ty, slice_ty)
            }
            ty::Slice(inner_ty) => (inner_ty, expected_ty),
            _ => {
                if !expected_ty.references_error() {
                    self.error_expected_array_or_slice(span, expected_ty);
                }
                (tcx.types.err, tcx.types.err)
            }
        };

        for elt in before {
            self.check_pat(&elt, inner_ty, def_bm, discrim_span);
        }
        if let Some(slice) = slice {
            self.check_pat(&slice, slice_ty, def_bm, discrim_span);
        }
        for elt in after {
            self.check_pat(&elt, inner_ty, def_bm, discrim_span);
        }
        expected_ty
    }

    fn error_scrutinee_inconsistent_length(&self, span: Span, min_len: u64, size: u64) {
        struct_span_err!(
            self.tcx.sess,
            span,
            E0527,
            "pattern requires {} element{} but array has {}",
            min_len,
            pluralise!(min_len),
            size,
        )
        .span_label(span, format!("expected {} element{}", size, pluralise!(size)))
        .emit();
    }

    fn error_scrutinee_with_rest_inconsistent_length(&self, span: Span, min_len: u64, size: u64) {
        struct_span_err!(
            self.tcx.sess,
            span,
            E0528,
            "pattern requires at least {} element{} but array has {}",
            min_len,
            pluralise!(min_len),
            size,
        ).span_label(
            span,
            format!(
                "pattern cannot match array of {} element{}",
                size,
                pluralise!(size),
            ),
        ).emit();
    }

    fn error_scrutinee_unfixed_length(&self, span: Span) {
        struct_span_err!(
            self.tcx.sess, span, E0730,
            "cannot pattern-match on an array without a fixed length",
        )
        .emit();
    }

    fn error_expected_array_or_slice(&self, span: Span, expected_ty: Ty<'tcx>) {
        let mut err = struct_span_err!(
            self.tcx.sess, span, E0529,
            "expected an array or slice, found `{}`",
            expected_ty
        );
        if let ty::Ref(_, ty, _) = expected_ty.sty {
            if let ty::Array(..) | ty::Slice(..) = ty.sty {
                err.help("the semantics of slice patterns changed recently; see issue #62254");
            }
        }
        err.span_label(span, format!("pattern cannot match with input type `{}`", expected_ty));
        err.emit();
    }
}
