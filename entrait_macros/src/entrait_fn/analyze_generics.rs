use super::attr::EntraitFnAttr;
use crate::generics::{FnDeps, FnGenerics, GenericIdents, TraitGenerics};
use crate::input::InputFn;

use syn::spanned::Spanned;

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

    // TODO: Should return FnDeps
    pub fn analyze_fn(&mut self, func: &InputFn, attr: &EntraitFnAttr) -> syn::Result<FnGenerics> {
        let span = attr.trait_ident.span();
        if attr.no_deps_value() {
            return Ok(FnGenerics::new(
                FnDeps::NoDeps {
                    idents: GenericIdents::new(span),
                },
                self.extract_type_generics(&func.fn_sig.generics),
            ));
        }

        let first_input =
            match func.fn_sig.inputs.first() {
                Some(fn_arg) => fn_arg,
                None => return Err(syn::Error::new(
                    func.fn_sig.ident.span(),
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

        self.extract_deps_from_type(span, func, pat_type, pat_type.ty.as_ref())
    }

    fn extract_deps_from_type<'f>(
        &mut self,
        span: proc_macro2::Span,
        func: &'f InputFn,
        arg_pat: &'f syn::PatType,
        ty: &'f syn::Type,
    ) -> syn::Result<FnGenerics> {
        match ty {
            syn::Type::ImplTrait(type_impl_trait) => {
                // Simple case, bounds are actually inline, no lookup necessary
                Ok(FnGenerics::new(
                    FnDeps::Generic {
                        generic_param: None,
                        trait_bounds: extract_trait_bounds(&type_impl_trait.bounds),
                        idents: GenericIdents::new(span),
                    },
                    self.extract_type_generics(&func.fn_sig.generics),
                ))
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
                    return Ok(FnGenerics::new(
                        FnDeps::Concrete(Box::new(ty.clone())),
                        self.extract_type_generics(&func.fn_sig.generics),
                    ));
                }

                let first_segment = type_path.path.segments.first().unwrap();

                match self.find_deps_generic_bounds(span, func, &first_segment.ident) {
                    Some(generics) => Ok(generics),
                    None => Ok(FnGenerics::new(
                        FnDeps::Concrete(Box::new(ty.clone())),
                        self.extract_type_generics(&func.fn_sig.generics),
                    )),
                }
            }
            syn::Type::Reference(type_reference) => {
                self.extract_deps_from_type(span, func, arg_pat, type_reference.elem.as_ref())
            }
            syn::Type::Paren(paren) => {
                self.extract_deps_from_type(span, func, arg_pat, paren.elem.as_ref())
            }
            ty => Ok(FnGenerics::new(
                FnDeps::Concrete(Box::new(ty.clone())),
                self.extract_type_generics(&func.fn_sig.generics),
            )),
        }
    }

    fn find_deps_generic_bounds(
        &mut self,
        span: proc_macro2::Span,
        func: &InputFn,
        generic_param_ident: &syn::Ident,
    ) -> Option<FnGenerics> {
        let generics = &func.fn_sig.generics;
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

        let mut remaining_params =
            syn::punctuated::Punctuated::<syn::GenericParam, syn::token::Comma>::new();

        for (index, param) in generic_params.iter().enumerate() {
            if index != matching_index {
                remaining_params.push(param.clone());
            }
        }

        // Extract "direct" bounds, not from where clause
        let mut deps_trait_bounds = extract_trait_bounds(&matching_type_param.bounds);

        // Check the where clause too
        let new_where_clause = if let Some(where_clause) = &generics.where_clause {
            let mut new_predicates =
                syn::punctuated::Punctuated::<syn::WherePredicate, syn::token::Comma>::new();

            for predicate in &where_clause.predicates {
                match predicate {
                    syn::WherePredicate::Type(predicate_type) => match &predicate_type.bounded_ty {
                        syn::Type::Path(type_path) => {
                            if type_path.qself.is_some() || type_path.path.leading_colon.is_some() {
                                new_predicates.push(predicate.clone());
                                continue;
                            }
                            if type_path.path.segments.len() != 1 {
                                new_predicates.push(predicate.clone());
                                continue;
                            }
                            let first_segment = type_path.path.segments.first().unwrap();

                            if &first_segment.ident == generic_param_ident {
                                let where_paths = extract_trait_bounds(&predicate_type.bounds);

                                deps_trait_bounds.extend(where_paths);
                            }
                        }
                        _ => {
                            new_predicates.push(predicate.clone());
                        }
                    },
                    _ => {
                        new_predicates.push(predicate.clone());
                    }
                }
            }

            if !new_predicates.is_empty() {
                Some(syn::WhereClause {
                    where_token: where_clause.where_token,
                    predicates: new_predicates,
                })
            } else {
                None
            }
        } else {
            None
        };

        let has_modified_generics = !remaining_params.is_empty() || new_where_clause.is_some();

        let modified_generics = syn::Generics {
            lt_token: generics.lt_token.filter(|_| has_modified_generics),
            params: remaining_params,
            where_clause: new_where_clause,
            gt_token: generics.gt_token.filter(|_| has_modified_generics),
        };

        Some(FnGenerics::new(
            FnDeps::Generic {
                generic_param: Some(generic_param_ident.clone()),
                trait_bounds: deps_trait_bounds,
                idents: GenericIdents::new(span),
            },
            modified_generics,
        ))
    }

    fn extract_type_generics(&mut self, generics: &syn::Generics) -> syn::Generics {
        if let Some(where_clause) = &generics.where_clause {
            for predicate in &where_clause.predicates {
                self.trait_generics.where_predicates.push(predicate.clone());
            }
        }

        let mut type_generics = syn::Generics {
            lt_token: generics.lt_token,
            params: syn::punctuated::Punctuated::new(),
            where_clause: generics.where_clause.clone(),
            gt_token: generics.gt_token,
        };

        for param in &generics.params {
            match param {
                syn::GenericParam::Type(_) => {
                    type_generics.params.push(param.clone());
                    self.trait_generics.params.push(param.clone());
                }
                syn::GenericParam::Const(_) => {
                    type_generics.params.push(param.clone());
                    self.trait_generics.params.push(param.clone());
                }
                syn::GenericParam::Lifetime(_) => {}
            }
        }

        type_generics
    }
}

fn extract_trait_bounds(
    bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, syn::token::Add>,
) -> Vec<syn::TypeParamBound> {
    bounds.iter().cloned().collect()
}
