// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Coherence phase
//
// The job of the coherence phase of typechecking is to ensure that
// each trait has at most one implementation for each type. This is
// done by the orphan and overlap modules. Then we build up various
// mappings. That mapping code resides here.


use middle::lang_items::UnsizeTraitLangItem;
use middle::subst::{self, Subst};
use middle::traits;
use middle::ty::RegionEscape;
use middle::ty::{ImplContainer, ImplOrTraitItemId, ConstTraitItemId};
use middle::ty::{MethodTraitItemId, TypeTraitItemId, ParameterEnvironment};
use middle::ty::{Ty, ty_bool, ty_char, ty_enum, ty_err};
use middle::ty::{ty_param, TypeScheme, ty_ptr};
use middle::ty::{ty_rptr, ty_struct, ty_trait, ty_tup};
use middle::ty::{ty_str, ty_vec, ty_float, ty_infer, ty_int};
use middle::ty::{ty_uint, ty_closure, ty_uniq, ty_bare_fn};
use middle::ty::ty_projection;
use middle::ty;
use middle::free_region::FreeRegionMap;
use CrateCtxt;
use middle::infer::{self, InferCtxt, new_infer_ctxt};
use std::cell::RefCell;
use std::rc::Rc;
use syntax::ast::{Crate, DefId};
use syntax::ast::{Item, ItemImpl};
use syntax::ast::{LOCAL_CRATE};
use syntax::ast;
use syntax::ast_map::NodeItem;
use syntax::ast_map;
use syntax::ast_util::local_def;
use syntax::codemap::Span;
use syntax::parse::token;
use syntax::visit;
use util::nodemap::{DefIdMap, FnvHashMap};
use util::ppaux::Repr;

mod orphan;
mod overlap;
mod unsafety;

// Returns the def ID of the base type, if there is one.
fn get_base_type_def_id<'a, 'tcx>(inference_context: &InferCtxt<'a, 'tcx>,
                                  span: Span,
                                  ty: Ty<'tcx>)
                                  -> Option<DefId> {
    match ty.sty {
        ty_enum(def_id, _) |
        ty_struct(def_id, _) => {
            Some(def_id)
        }

        ty_trait(ref t) => {
            Some(t.principal_def_id())
        }

        ty_uniq(_) => {
            inference_context.tcx.lang_items.owned_box()
        }

        ty_bool | ty_char | ty_int(..) | ty_uint(..) | ty_float(..) |
        ty_str(..) | ty_vec(..) | ty_bare_fn(..) | ty_tup(..) |
        ty_param(..) | ty_err |
        ty_ptr(_) | ty_rptr(_, _) | ty_projection(..) => {
            None
        }

        ty_infer(..) | ty_closure(..) => {
            // `ty` comes from a user declaration so we should only expect types
            // that the user can type
            inference_context.tcx.sess.span_bug(
                span,
                &format!("coherence encountered unexpected type searching for base type: {}",
                        ty.repr(inference_context.tcx)));
        }
    }
}

struct CoherenceChecker<'a, 'tcx: 'a> {
    crate_context: &'a CrateCtxt<'a, 'tcx>,
    inference_context: InferCtxt<'a, 'tcx>,
    inherent_impls: RefCell<DefIdMap<Rc<RefCell<Vec<ast::DefId>>>>>,
}

struct CoherenceCheckVisitor<'a, 'tcx: 'a> {
    cc: &'a CoherenceChecker<'a, 'tcx>
}

impl<'a, 'tcx, 'v> visit::Visitor<'v> for CoherenceCheckVisitor<'a, 'tcx> {
    fn visit_item(&mut self, item: &Item) {
        if let ItemImpl(..) = item.node {
            self.cc.check_implementation(item)
        }

        visit::walk_item(self, item);
    }
}

impl<'a, 'tcx> CoherenceChecker<'a, 'tcx> {
    fn check(&self, krate: &Crate) {
        // Check implementations and traits. This populates the tables
        // containing the inherent methods and extension methods. It also
        // builds up the trait inheritance table.
        let mut visitor = CoherenceCheckVisitor { cc: self };
        visit::walk_crate(&mut visitor, krate);

        // Copy over the inherent impls we gathered up during the walk into
        // the tcx.
        let mut tcx_inherent_impls =
            self.crate_context.tcx.inherent_impls.borrow_mut();
        for (k, v) in &*self.inherent_impls.borrow() {
            tcx_inherent_impls.insert((*k).clone(),
                                      Rc::new((*v.borrow()).clone()));
        }

        // Populate the table of destructors. It might seem a bit strange to
        // do this here, but it's actually the most convenient place, since
        // the coherence tables contain the trait -> type mappings.
        self.populate_destructor_table();

        // Check to make sure implementations of `Copy` are legal.
        self.check_implementations_of_copy();

        // Check to make sure implementations of `CoerceUnsized` are legal
        // and collect the necessary information from them.
        self.check_implementations_of_coerce_unsized();
    }

    fn check_implementation(&self, item: &Item) {
        let tcx = self.crate_context.tcx;
        let impl_did = local_def(item.id);
        let self_type = ty::lookup_item_type(tcx, impl_did);

        // If there are no traits, then this implementation must have a
        // base type.

        let impl_items = self.create_impl_from_item(item);

        if let Some(trait_ref) = ty::impl_trait_ref(self.crate_context.tcx,
                                                    impl_did) {
            debug!("(checking implementation) adding impl for trait '{}', item '{}'",
                   trait_ref.repr(self.crate_context.tcx),
                   token::get_ident(item.ident));

            enforce_trait_manually_implementable(self.crate_context.tcx,
                                                 item.span,
                                                 trait_ref.def_id);
            self.add_trait_impl(trait_ref, impl_did);
        } else {
            // Add the implementation to the mapping from implementation to base
            // type def ID, if there is a base type for this implementation and
            // the implementation does not have any associated traits.
            if let Some(base_type_def_id) = get_base_type_def_id(
                    &self.inference_context, item.span, self_type.ty) {
                self.add_inherent_impl(base_type_def_id, impl_did);
            }
        }

        tcx.impl_items.borrow_mut().insert(impl_did, impl_items);
    }

    // Creates default method IDs and performs type substitutions for an impl
    // and trait pair. Then, for each provided method in the trait, inserts a
    // `ProvidedMethodInfo` instance into the `provided_method_sources` map.
    fn instantiate_default_methods(
            &self,
            impl_id: DefId,
            trait_ref: &ty::TraitRef<'tcx>,
            all_impl_items: &mut Vec<ImplOrTraitItemId>) {
        let tcx = self.crate_context.tcx;
        debug!("instantiate_default_methods(impl_id={:?}, trait_ref={})",
               impl_id, trait_ref.repr(tcx));

        let impl_type_scheme = ty::lookup_item_type(tcx, impl_id);

        let prov = ty::provided_trait_methods(tcx, trait_ref.def_id);
        for trait_method in &prov {
            // Synthesize an ID.
            let new_id = tcx.sess.next_node_id();
            let new_did = local_def(new_id);

            debug!("new_did={:?} trait_method={}", new_did, trait_method.repr(tcx));

            // Create substitutions for the various trait parameters.
            let new_method_ty =
                Rc::new(subst_receiver_types_in_method_ty(
                    tcx,
                    impl_id,
                    &impl_type_scheme,
                    trait_ref,
                    new_did,
                    &**trait_method,
                    Some(trait_method.def_id)));

            debug!("new_method_ty={}", new_method_ty.repr(tcx));
            all_impl_items.push(MethodTraitItemId(new_did));

            // construct the polytype for the method based on the
            // method_ty.  it will have all the generics from the
            // impl, plus its own.
            let new_polytype = ty::TypeScheme {
                generics: new_method_ty.generics.clone(),
                ty: ty::mk_bare_fn(tcx, Some(new_did),
                                   tcx.mk_bare_fn(new_method_ty.fty.clone()))
            };
            debug!("new_polytype={}", new_polytype.repr(tcx));

            tcx.tcache.borrow_mut().insert(new_did, new_polytype);
            tcx.predicates.borrow_mut().insert(new_did, new_method_ty.predicates.clone());
            tcx.impl_or_trait_items
               .borrow_mut()
               .insert(new_did, ty::MethodTraitItem(new_method_ty));

            // Pair the new synthesized ID up with the
            // ID of the method.
            self.crate_context.tcx.provided_method_sources.borrow_mut()
                .insert(new_did, trait_method.def_id);
        }
    }

    fn add_inherent_impl(&self, base_def_id: DefId, impl_def_id: DefId) {
        match self.inherent_impls.borrow().get(&base_def_id) {
            Some(implementation_list) => {
                implementation_list.borrow_mut().push(impl_def_id);
                return;
            }
            None => {}
        }

        self.inherent_impls.borrow_mut().insert(
            base_def_id,
            Rc::new(RefCell::new(vec!(impl_def_id))));
    }

    fn add_trait_impl(&self, impl_trait_ref: ty::TraitRef<'tcx>, impl_def_id: DefId) {
        debug!("add_trait_impl: impl_trait_ref={:?} impl_def_id={:?}",
               impl_trait_ref, impl_def_id);
        let trait_def = ty::lookup_trait_def(self.crate_context.tcx,
                                             impl_trait_ref.def_id);
        trait_def.record_impl(self.crate_context.tcx, impl_def_id, impl_trait_ref);
    }

    // Converts an implementation in the AST to a vector of items.
    fn create_impl_from_item(&self, item: &Item) -> Vec<ImplOrTraitItemId> {
        match item.node {
            ItemImpl(_, _, _, _, _, ref impl_items) => {
                let mut items: Vec<ImplOrTraitItemId> =
                        impl_items.iter().map(|impl_item| {
                    match impl_item.node {
                        ast::ConstImplItem(..) => {
                            ConstTraitItemId(local_def(impl_item.id))
                        }
                        ast::MethodImplItem(..) => {
                            MethodTraitItemId(local_def(impl_item.id))
                        }
                        ast::TypeImplItem(_) => {
                            TypeTraitItemId(local_def(impl_item.id))
                        }
                        ast::MacImplItem(_) => {
                            self.crate_context.tcx.sess.span_bug(impl_item.span,
                                                                 "unexpanded macro");
                        }
                    }
                }).collect();

                if let Some(trait_ref) = ty::impl_trait_ref(self.crate_context.tcx,
                                                            local_def(item.id)) {
                    self.instantiate_default_methods(local_def(item.id),
                                                     &trait_ref,
                                                     &mut items);
                }

                items
            }
            _ => {
                self.crate_context.tcx.sess.span_bug(item.span,
                                                     "can't convert a non-impl \
                                                      to an impl");
            }
        }
    }

    //
    // Destructors
    //

    fn populate_destructor_table(&self) {
        let tcx = self.crate_context.tcx;
        let drop_trait = match tcx.lang_items.drop_trait() {
            Some(id) => id, None => { return }
        };
        ty::populate_implementations_for_trait_if_necessary(tcx, drop_trait);
        let drop_trait = ty::lookup_trait_def(tcx, drop_trait);

        let impl_items = tcx.impl_items.borrow();

        drop_trait.for_each_impl(tcx, |impl_did| {
            let items = impl_items.get(&impl_did).unwrap();
            if items.is_empty() {
                // We'll error out later. For now, just don't ICE.
                return;
            }
            let method_def_id = items[0];

            let self_type = ty::lookup_item_type(tcx, impl_did);
            match self_type.ty.sty {
                ty::ty_enum(type_def_id, _) |
                ty::ty_struct(type_def_id, _) |
                ty::ty_closure(type_def_id, _) => {
                    tcx.destructor_for_type
                       .borrow_mut()
                       .insert(type_def_id, method_def_id.def_id());
                    tcx.destructors
                       .borrow_mut()
                       .insert(method_def_id.def_id());
                }
                _ => {
                    // Destructors only work on nominal types.
                    if impl_did.krate == ast::LOCAL_CRATE {
                        {
                            match tcx.map.find(impl_did.node) {
                                Some(ast_map::NodeItem(item)) => {
                                    span_err!(tcx.sess, item.span, E0120,
                                        "the Drop trait may only be implemented on structures");
                                }
                                _ => {
                                    tcx.sess.bug("didn't find impl in ast \
                                                  map");
                                }
                            }
                        }
                    } else {
                        tcx.sess.bug("found external impl of Drop trait on \
                                      something other than a struct");
                    }
                }
            }
        });
    }

    /// Ensures that implementations of the built-in trait `Copy` are legal.
    fn check_implementations_of_copy(&self) {
        let tcx = self.crate_context.tcx;
        let copy_trait = match tcx.lang_items.copy_trait() {
            Some(id) => id,
            None => return,
        };
        ty::populate_implementations_for_trait_if_necessary(tcx, copy_trait);
        let copy_trait = ty::lookup_trait_def(tcx, copy_trait);

        copy_trait.for_each_impl(tcx, |impl_did| {
            debug!("check_implementations_of_copy: impl_did={}",
                   impl_did.repr(tcx));

            if impl_did.krate != ast::LOCAL_CRATE {
                debug!("check_implementations_of_copy(): impl not in this \
                        crate");
                return
            }

            let self_type = ty::lookup_item_type(tcx, impl_did);
            debug!("check_implementations_of_copy: self_type={} (bound)",
                   self_type.repr(tcx));

            let span = tcx.map.span(impl_did.node);
            let param_env = ParameterEnvironment::for_item(tcx, impl_did.node);
            let self_type = self_type.ty.subst(tcx, &param_env.free_substs);
            assert!(!self_type.has_escaping_regions());

            debug!("check_implementations_of_copy: self_type={} (free)",
                   self_type.repr(tcx));

            match ty::can_type_implement_copy(&param_env, span, self_type) {
                Ok(()) => {}
                Err(ty::FieldDoesNotImplementCopy(name)) => {
                       span_err!(tcx.sess, span, E0204,
                                 "the trait `Copy` may not be \
                                          implemented for this type; field \
                                          `{}` does not implement `Copy`",
                                         token::get_name(name))
                }
                Err(ty::VariantDoesNotImplementCopy(name)) => {
                       span_err!(tcx.sess, span, E0205,
                                 "the trait `Copy` may not be \
                                          implemented for this type; variant \
                                          `{}` does not implement `Copy`",
                                         token::get_name(name))
                }
                Err(ty::TypeIsStructural) => {
                       span_err!(tcx.sess, span, E0206,
                                 "the trait `Copy` may not be implemented \
                                  for this type; type is not a structure or \
                                  enumeration")
                }
                Err(ty::TypeHasDestructor) => {
                    span_err!(tcx.sess, span, E0184,
                              "the trait `Copy` may not be implemented for this type; \
                               the type has a destructor");
                }
            }
        });
    }

    /// Process implementations of the built-in trait `CoerceUnsized`.
    fn check_implementations_of_coerce_unsized(&self) {
        let tcx = self.crate_context.tcx;
        let coerce_unsized_trait = match tcx.lang_items.coerce_unsized_trait() {
            Some(id) => id,
            None => return,
        };
        let unsize_trait = match tcx.lang_items.require(UnsizeTraitLangItem) {
            Ok(id) => id,
            Err(err) => {
                tcx.sess.fatal(&format!("`CoerceUnsized` implementation {}", err));
            }
        };

        let trait_def = ty::lookup_trait_def(tcx, coerce_unsized_trait);

        trait_def.for_each_impl(tcx, |impl_did| {
            debug!("check_implementations_of_coerce_unsized: impl_did={}",
                   impl_did.repr(tcx));

            if impl_did.krate != ast::LOCAL_CRATE {
                debug!("check_implementations_of_coerce_unsized(): impl not \
                        in this crate");
                return;
            }

            let source = ty::lookup_item_type(tcx, impl_did).ty;
            let trait_ref = ty::impl_trait_ref(self.crate_context.tcx,
                                               impl_did).unwrap();
            let target = *trait_ref.substs.types.get(subst::TypeSpace, 0);
            debug!("check_implementations_of_coerce_unsized: {} -> {} (bound)",
                   source.repr(tcx), target.repr(tcx));

            let span = tcx.map.span(impl_did.node);
            let param_env = ParameterEnvironment::for_item(tcx, impl_did.node);
            let source = source.subst(tcx, &param_env.free_substs);
            let target = target.subst(tcx, &param_env.free_substs);
            assert!(!source.has_escaping_regions());

            debug!("check_implementations_of_coerce_unsized: {} -> {} (free)",
                   source.repr(tcx), target.repr(tcx));

            let infcx = new_infer_ctxt(tcx);

            let check_mutbl = |mt_a: ty::mt<'tcx>, mt_b: ty::mt<'tcx>,
                               mk_ptr: &Fn(Ty<'tcx>) -> Ty<'tcx>| {
                if (mt_a.mutbl, mt_b.mutbl) == (ast::MutImmutable, ast::MutMutable) {
                    infcx.report_mismatched_types(span, mk_ptr(mt_b.ty),
                                                  target, &ty::terr_mutability);
                }
                (mt_a.ty, mt_b.ty, unsize_trait, None)
            };
            let (source, target, trait_def_id, kind) = match (&source.sty, &target.sty) {
                (&ty::ty_uniq(a), &ty::ty_uniq(b)) => (a, b, unsize_trait, None),

                (&ty::ty_rptr(r_a, mt_a), &ty::ty_rptr(r_b, mt_b)) => {
                    infer::mk_subr(&infcx, infer::RelateObjectBound(span), *r_b, *r_a);
                    check_mutbl(mt_a, mt_b, &|ty| ty::mk_imm_rptr(tcx, r_b, ty))
                }

                (&ty::ty_rptr(_, mt_a), &ty::ty_ptr(mt_b)) |
                (&ty::ty_ptr(mt_a), &ty::ty_ptr(mt_b)) => {
                    check_mutbl(mt_a, mt_b, &|ty| ty::mk_imm_ptr(tcx, ty))
                }

                (&ty::ty_struct(def_id_a, substs_a), &ty::ty_struct(def_id_b, substs_b)) => {
                    if def_id_a != def_id_b {
                        let source_path = ty::item_path_str(tcx, def_id_a);
                        let target_path = ty::item_path_str(tcx, def_id_b);
                        span_err!(tcx.sess, span, E0377,
                                  "the trait `CoerceUnsized` may only be implemented \
                                   for a coercion between structures with the same \
                                   definition; expected {}, found {}",
                                  source_path, target_path);
                        return;
                    }

                    let origin = infer::Misc(span);
                    let fields = ty::lookup_struct_fields(tcx, def_id_a);
                    let diff_fields = fields.iter().enumerate().filter_map(|(i, f)| {
                        let ty = ty::lookup_field_type_unsubstituted(tcx, def_id_a, f.id);
                        let (a, b) = (ty.subst(tcx, substs_a), ty.subst(tcx, substs_b));
                        if infcx.sub_types(false, origin, b, a).is_ok() {
                            None
                        } else {
                            Some((i, a, b))
                        }
                    }).collect::<Vec<_>>();

                    if diff_fields.is_empty() {
                        span_err!(tcx.sess, span, E0374,
                                  "the trait `CoerceUnsized` may only be implemented \
                                   for a coercion between structures with one field \
                                   being coerced, none found");
                        return;
                    } else if diff_fields.len() > 1 {
                        span_err!(tcx.sess, span, E0375,
                                  "the trait `CoerceUnsized` may only be implemented \
                                   for a coercion between structures with one field \
                                   being coerced, but {} fields need coercions: {}",
                                   diff_fields.len(), diff_fields.iter().map(|&(i, a, b)| {
                                        let name = fields[i].name;
                                        format!("{} ({} to {})",
                                                if name == token::special_names::unnamed_field {
                                                    i.to_string()
                                                } else {
                                                    token::get_name(name).to_string()
                                                },
                                                a.repr(tcx),
                                                b.repr(tcx))
                                   }).collect::<Vec<_>>().connect(", "));
                        return;
                    }

                    let (i, a, b) = diff_fields[0];
                    let kind = ty::CustomCoerceUnsized::Struct(i);
                    (a, b, coerce_unsized_trait, Some(kind))
                }

                _ => {
                    span_err!(tcx.sess, span, E0376,
                              "the trait `CoerceUnsized` may only be implemented \
                               for a coercion between structures");
                    return;
                }
            };

            let mut fulfill_cx = traits::FulfillmentContext::new();

            // Register an obligation for `A: Trait<B>`.
            let cause = traits::ObligationCause::misc(span, impl_did.node);
            let predicate = traits::predicate_for_trait_def(tcx, cause, trait_def_id,
                                                            0, source, vec![target]);
            fulfill_cx.register_predicate_obligation(&infcx, predicate);

            // Check that all transitive obligations are satisfied.
            if let Err(errors) = fulfill_cx.select_all_or_error(&infcx, &param_env) {
                traits::report_fulfillment_errors(&infcx, &errors);
            }

            // Finally, resolve all regions.
            let mut free_regions = FreeRegionMap::new();
            free_regions.relate_free_regions_from_predicates(tcx, &param_env.caller_bounds);
            infcx.resolve_regions_and_report_errors(&free_regions, impl_did.node);

            if let Some(kind) = kind {
                tcx.custom_coerce_unsized_kinds.borrow_mut().insert(impl_did, kind);
            }
        });
    }
}

fn enforce_trait_manually_implementable(tcx: &ty::ctxt, sp: Span, trait_def_id: ast::DefId) {
    if tcx.sess.features.borrow().unboxed_closures {
        // the feature gate allows all of them
        return
    }
    let did = Some(trait_def_id);
    let li = &tcx.lang_items;

    let trait_name = if did == li.fn_trait() {
        "Fn"
    } else if did == li.fn_mut_trait() {
        "FnMut"
    } else if did == li.fn_once_trait() {
        "FnOnce"
    } else {
        return // everything OK
    };
    span_err!(tcx.sess, sp, E0183, "manual implementations of `{}` are experimental", trait_name);
    fileline_help!(tcx.sess, sp,
               "add `#![feature(unboxed_closures)]` to the crate attributes to enable");
}

fn subst_receiver_types_in_method_ty<'tcx>(tcx: &ty::ctxt<'tcx>,
                                           impl_id: ast::DefId,
                                           impl_type_scheme: &ty::TypeScheme<'tcx>,
                                           trait_ref: &ty::TraitRef<'tcx>,
                                           new_def_id: ast::DefId,
                                           method: &ty::Method<'tcx>,
                                           provided_source: Option<ast::DefId>)
                                           -> ty::Method<'tcx>
{
    let combined_substs = ty::make_substs_for_receiver_types(tcx, trait_ref, method);

    debug!("subst_receiver_types_in_method_ty: combined_substs={}",
           combined_substs.repr(tcx));

    let method_predicates = method.predicates.subst(tcx, &combined_substs);
    let mut method_generics = method.generics.subst(tcx, &combined_substs);

    // replace the type parameters declared on the trait with those
    // from the impl
    for &space in &[subst::TypeSpace, subst::SelfSpace] {
        method_generics.types.replace(
            space,
            impl_type_scheme.generics.types.get_slice(space).to_vec());
        method_generics.regions.replace(
            space,
            impl_type_scheme.generics.regions.get_slice(space).to_vec());
    }

    debug!("subst_receiver_types_in_method_ty: method_generics={}",
           method_generics.repr(tcx));

    let method_fty = method.fty.subst(tcx, &combined_substs);

    debug!("subst_receiver_types_in_method_ty: method_ty={}",
           method.fty.repr(tcx));

    ty::Method::new(
        method.name,
        method_generics,
        method_predicates,
        method_fty,
        method.explicit_self,
        method.vis,
        new_def_id,
        ImplContainer(impl_id),
        provided_source
    )
}

pub fn check_coherence(crate_context: &CrateCtxt) {
    CoherenceChecker {
        crate_context: crate_context,
        inference_context: new_infer_ctxt(crate_context.tcx),
        inherent_impls: RefCell::new(FnvHashMap()),
    }.check(crate_context.tcx.map.krate());
    unsafety::check(crate_context.tcx);
    orphan::check(crate_context.tcx);
    overlap::check(crate_context.tcx);
}
