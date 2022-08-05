use crate::generics::{FnDeps, TraitDependencyMode, TraitGenerics};
use crate::idents::{CrateIdents, GenericIdents};
use crate::input::FnInputMode;
use crate::opt::Opts;
use crate::signature::{
    EntraitSignature, FnIndex, InjectDynImplParam, InputSig, SignatureConverter,
};

use proc_macro2::Span;
use syn::spanned::Spanned;

pub struct TraitFn<'i> {
    pub input_sig: InputSig<'i>,
    pub deps: FnDeps,
    pub entrait_sig: EntraitSignature,
}

impl<'i> TraitFn<'i> {
    pub fn sig(&self) -> &syn::Signature {
        &self.entrait_sig.sig
    }
}

pub struct TraitFnAnalyzer<'s> {
    pub inject_dyn_impl_param: InjectDynImplParam,
    pub trait_span: Span,
    pub crate_idents: &'s CrateIdents,
    pub opts: &'s Opts,
}

impl<'s> TraitFnAnalyzer<'s> {
    pub fn analyze<'i>(
        self,
        input_sig: InputSig<'i>,
        fn_index: FnIndex,
        analyzer: &mut GenericsAnalyzer,
    ) -> syn::Result<TraitFn<'i>> {
        let deps = analyzer.analyze_fn_deps(input_sig, self.trait_span, self.opts)?;
        let entrait_sig = SignatureConverter {
            crate_idents: self.crate_idents,
            trait_span: self.trait_span,
            opts: self.opts,
            input_sig,
            deps: &deps,
            fn_index,
            inject_dyn_impl_param: self.inject_dyn_impl_param,
        }
        .convert();
        Ok(TraitFn {
            input_sig,
            deps,
            entrait_sig,
        })
    }
}

pub(super) fn detect_trait_dependency_mode<'t, 'c>(
    input_mode: &FnInputMode,
    trait_fns: &'t [TraitFn],
    crate_idents: &'c CrateIdents,
    span: proc_macro2::Span,
) -> syn::Result<TraitDependencyMode<'t, 'c>> {
    for trait_fn in trait_fns {
        if let FnDeps::Concrete(ty) = &trait_fn.deps {
            return match input_mode {
                FnInputMode::SingleFn(_) => Ok(TraitDependencyMode::Concrete(ty.as_ref())),
                FnInputMode::Module(_) => Err(syn::Error::new(
                    ty.span(),
                    "Using concrete dependencies in a module is an anti-pattern. Instead, write a trait manually, use the #[entrait] attribute on it, and implement it for your application type",
                )),
            };
        }
    }

    Ok(TraitDependencyMode::Generic(GenericIdents::new(
        crate_idents,
        span,
    )))
}

pub struct GenericsAnalyzer {
    trait_generics: TraitGenerics,
}

impl GenericsAnalyzer {
    pub fn new() -> Self {
        Self {
            trait_generics: TraitGenerics {
                params: Default::default(),
                where_predicates: Default::default(),
            },
        }
    }

    pub fn into_trait_generics(self) -> TraitGenerics {
        self.trait_generics
    }

    pub fn analyze_fn_deps(
        &mut self,
        input_sig: InputSig<'_>,
        trait_span: proc_macro2::Span,
        opts: &Opts,
    ) -> syn::Result<FnDeps> {
        if opts.no_deps_value() {
            return self.deps_with_generics(FnDeps::NoDeps, &input_sig.generics);
        }

        let first_input =
            match input_sig.inputs.first() {
                Some(fn_arg) => fn_arg,
                None => return Err(syn::Error::new(
                    input_sig.ident.span(),
                    "Function must have a dependency 'receiver' as its first parameter. Pass `no_deps` to entrait to disable dependency injection.",
                )),
            };

        let pat_type = match first_input {
            syn::FnArg::Typed(pat_type) => pat_type,
            syn::FnArg::Receiver(_) => {
                return Err(syn::Error::new(
                    first_input.span(),
                    "Function cannot have a self receiver",
                ))
            }
        };

        self.extract_deps_from_type(trait_span, input_sig, pat_type, pat_type.ty.as_ref())
    }

    fn extract_deps_from_type(
        &mut self,
        trait_span: proc_macro2::Span,
        input_sig: InputSig<'_>,
        arg_pat: &syn::PatType,
        ty: &syn::Type,
    ) -> syn::Result<FnDeps> {
        match ty {
            syn::Type::ImplTrait(type_impl_trait) => {
                // Simple case, bounds are actually inline, no lookup necessary
                self.deps_with_generics(
                    FnDeps::Generic {
                        generic_param: None,
                        trait_bounds: extract_trait_bounds(&type_impl_trait.bounds),
                    },
                    &input_sig.generics,
                )
            }
            syn::Type::Path(type_path) => {
                // Type path. Should be defined as a generic parameter.
                if type_path.qself.is_some() {
                    return Err(syn::Error::new(type_path.span(), "No self allowed"));
                }
                if type_path.path.leading_colon.is_some() {
                    return Err(syn::Error::new(
                        type_path.span(),
                        "No leading colon allowed",
                    ));
                }
                if type_path.path.segments.len() != 1 {
                    return self.deps_with_generics(
                        FnDeps::Concrete(Box::new(ty.clone())),
                        &input_sig.generics,
                    );
                }

                let first_segment = type_path.path.segments.first().unwrap();

                match self.find_deps_generic_bounds(input_sig, &first_segment.ident) {
                    Some(generics) => Ok(generics),
                    None => self.deps_with_generics(
                        FnDeps::Concrete(Box::new(ty.clone())),
                        &input_sig.generics,
                    ),
                }
            }
            syn::Type::Reference(type_reference) => self.extract_deps_from_type(
                trait_span,
                input_sig,
                arg_pat,
                type_reference.elem.as_ref(),
            ),
            syn::Type::Paren(paren) => {
                self.extract_deps_from_type(trait_span, input_sig, arg_pat, paren.elem.as_ref())
            }
            ty => {
                self.deps_with_generics(FnDeps::Concrete(Box::new(ty.clone())), &input_sig.generics)
            }
        }
    }

    fn find_deps_generic_bounds(
        &mut self,
        input_sig: InputSig<'_>,
        generic_param_ident: &syn::Ident,
    ) -> Option<FnDeps> {
        let generics = &input_sig.generics;
        let generic_params = &generics.params;

        let (matching_index, matching_type_param) = generic_params
            .into_iter()
            .enumerate()
            .find_map(|(index, param)| match param {
                syn::GenericParam::Type(type_param) => {
                    if &type_param.ident == generic_param_ident {
                        Some((index, type_param))
                    } else {
                        None
                    }
                }
                _ => None,
            })?;

        for (index, param) in generic_params.iter().enumerate() {
            if index != matching_index {
                self.trait_generics.params.push(param.clone());
            }
        }

        // Extract "direct" bounds, not from where clause
        let mut deps_trait_bounds = extract_trait_bounds(&matching_type_param.bounds);

        if let Some(where_clause) = &generics.where_clause {
            for predicate in &where_clause.predicates {
                match predicate {
                    syn::WherePredicate::Type(predicate_type) => match &predicate_type.bounded_ty {
                        syn::Type::Path(type_path) => {
                            if type_path.qself.is_some() || type_path.path.leading_colon.is_some() {
                                self.trait_generics.where_predicates.push(predicate.clone());
                                continue;
                            }
                            if type_path.path.segments.len() != 1 {
                                self.trait_generics.where_predicates.push(predicate.clone());
                                continue;
                            }
                            let first_segment = type_path.path.segments.first().unwrap();

                            if &first_segment.ident == generic_param_ident {
                                let where_paths = extract_trait_bounds(&predicate_type.bounds);

                                deps_trait_bounds.extend(where_paths);
                            }
                        }
                        _ => {
                            self.trait_generics.where_predicates.push(predicate.clone());
                        }
                    },
                    _ => {
                        self.trait_generics.where_predicates.push(predicate.clone());
                    }
                }
            }
        };

        Some(FnDeps::Generic {
            generic_param: Some(generic_param_ident.clone()),
            trait_bounds: deps_trait_bounds,
        })
    }

    fn deps_with_generics(
        &mut self,
        deps: FnDeps,
        generics: &syn::Generics,
    ) -> syn::Result<FnDeps> {
        for param in &generics.params {
            match param {
                syn::GenericParam::Type(_) => {
                    self.trait_generics.params.push(param.clone());
                }
                syn::GenericParam::Const(_) => {
                    self.trait_generics.params.push(param.clone());
                }
                syn::GenericParam::Lifetime(_) => {}
            }
        }

        if let Some(where_clause) = &generics.where_clause {
            for predicate in &where_clause.predicates {
                self.trait_generics.where_predicates.push(predicate.clone());
            }
        }

        Ok(deps)
    }
}

fn extract_trait_bounds(
    bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, syn::token::Add>,
) -> Vec<syn::TypeParamBound> {
    bounds.iter().cloned().collect()
}
