use super::{fn_params, lifetimes, ReceiverGeneration, SigComponent};
use super::{EntraitSignature, FnIndex, InjectDynImplParam, InputSig};
use crate::{generics::FnDeps, idents::CrateIdents, opt::Opts};

use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};

pub struct SignatureConverter<'a> {
    pub crate_idents: &'a CrateIdents,
    pub trait_span: Span,
    pub opts: &'a Opts,
    pub input_sig: InputSig<'a>,
    pub deps: &'a FnDeps,
    pub inject_dyn_impl_param: InjectDynImplParam,
    pub fn_index: FnIndex,
}

impl<'a> SignatureConverter<'a> {
    /// Convert from an standalone `fn` signature to a trait `fn` signature.
    pub fn convert_fn_to_trait_fn(&self) -> EntraitSignature {
        let mut entrait_sig = EntraitSignature {
            sig: self.input_sig.sig.clone(),
            associated_fut_decl: None,
            associated_fut_impl: None,
            lifetimes: vec![],
        };

        // strip away attributes
        for fn_arg in entrait_sig.sig.inputs.iter_mut() {
            match fn_arg {
                syn::FnArg::Receiver(receiver) => {
                    receiver.attrs = vec![];
                }
                syn::FnArg::Typed(pat_type) => {
                    pat_type.attrs = vec![];
                }
            }
        }

        let receiver_generation = self.detect_receiver_generation(&entrait_sig.sig);
        self.generate_params(&mut entrait_sig.sig, receiver_generation);

        if self.input_sig.use_associated_future(self.opts) {
            self.convert_to_associated_future(&mut entrait_sig, receiver_generation);
        }

        self.remove_generic_type_params(&mut entrait_sig.sig);
        tidy_generics(&mut entrait_sig.sig.generics);

        fn_params::fix_fn_param_idents(&mut entrait_sig.sig);

        entrait_sig
    }

    fn detect_receiver_generation(&self, sig: &syn::Signature) -> ReceiverGeneration {
        match self.deps {
            FnDeps::NoDeps { .. } => ReceiverGeneration::Insert,
            _ => {
                if sig.inputs.is_empty() {
                    if self.input_sig.use_associated_future(self.opts) {
                        ReceiverGeneration::Insert
                    } else {
                        ReceiverGeneration::None // bug?
                    }
                } else {
                    ReceiverGeneration::Rewrite
                }
            }
        }
    }

    fn generate_params(&self, sig: &mut syn::Signature, receiver_generation: ReceiverGeneration) {
        let span = self.trait_span;
        match receiver_generation {
            ReceiverGeneration::Insert => {
                sig.inputs
                    .insert(0, syn::parse_quote_spanned! { span=> &self });
            }
            ReceiverGeneration::Rewrite => {
                let input = sig.inputs.first_mut().unwrap();
                match input {
                    syn::FnArg::Typed(pat_type) => match pat_type.ty.as_ref() {
                        syn::Type::Reference(type_reference) => {
                            let and_token = type_reference.and_token;
                            let lifetime = type_reference.lifetime.clone();

                            *input = syn::FnArg::Receiver(syn::Receiver {
                                attrs: vec![],
                                reference: Some((and_token, lifetime)),
                                mutability: None,
                                self_token: syn::parse_quote_spanned! { span=> self },
                            });
                        }
                        _ => {
                            let first_mut = sig.inputs.first_mut().unwrap();
                            *first_mut = syn::parse_quote_spanned! { span=> &self };
                        }
                    },
                    syn::FnArg::Receiver(_) => panic!(),
                }
            }
            ReceiverGeneration::None => {}
        }

        if self.inject_dyn_impl_param.0 {
            let entrait = &self.crate_idents.entrait;
            sig.inputs.insert(
                1,
                syn::parse_quote! {
                    __impl: &::#entrait::Impl<EntraitT>
                },
            );
        }
    }

    fn convert_to_associated_future(
        &self,
        entrait_sig: &mut EntraitSignature,
        receiver_generation: ReceiverGeneration,
    ) {
        let span = self.trait_span;

        lifetimes::de_elide_lifetimes(entrait_sig, receiver_generation);

        let output_ty = output_type_tokens(&entrait_sig.sig.output);

        let fut_lifetimes = entrait_sig
            .lifetimes
            .iter()
            .map(|ft| &ft.lifetime)
            .collect::<Vec<_>>();
        let self_lifetimes = entrait_sig
            .lifetimes
            .iter()
            .filter(|ft| ft.source == SigComponent::Receiver)
            .map(|ft| &ft.lifetime)
            .collect::<Vec<_>>();

        // make the function generic if it wasn't already
        let sig = &mut entrait_sig.sig;
        sig.asyncness = None;
        let generics = &mut sig.generics;
        generics.lt_token.get_or_insert(syn::parse_quote! { < });
        generics.gt_token.get_or_insert(syn::parse_quote! { > });

        // insert generated/non-user-provided lifetimes
        for fut_lifetime in entrait_sig
            .lifetimes
            .iter()
            .filter(|lt| !lt.user_provided.0)
        {
            generics
                .params
                .push(syn::GenericParam::Lifetime(syn::LifetimeDef {
                    attrs: vec![],
                    lifetime: fut_lifetime.lifetime.clone(),
                    colon_token: None,
                    bounds: syn::punctuated::Punctuated::new(),
                }));
        }

        let fut = quote::format_ident!("Fut{}", self.fn_index.0);

        entrait_sig.sig.output = syn::parse_quote_spanned! {span =>
            -> Self::#fut<#(#fut_lifetimes),*>
        };

        let core = &self.crate_idents.core;

        entrait_sig.associated_fut_decl = Some(quote_spanned! { span=>
            type #fut<#(#fut_lifetimes),*>: ::#core::future::Future<Output = #output_ty> + Send
            where
                Self: #(#self_lifetimes)+*;
        });

        entrait_sig.associated_fut_impl = Some(quote_spanned! { span=>
            type #fut<#(#fut_lifetimes),*> = impl ::#core::future::Future<Output = #output_ty>
            where
                Self: #(#self_lifetimes)+*;
        });
    }

    fn remove_generic_type_params(&self, sig: &mut syn::Signature) {
        let deps_ident = match &self.deps {
            FnDeps::Generic { generic_param, .. } => generic_param.as_ref(),
            _ => None,
        };

        let generics = &mut sig.generics;
        let mut params = syn::punctuated::Punctuated::new();
        std::mem::swap(&mut params, &mut generics.params);

        for param in params.into_iter() {
            match &param {
                syn::GenericParam::Type(_) => {}
                _ => {
                    generics.params.push(param);
                }
            }
        }

        if let Some(where_clause) = &mut generics.where_clause {
            let mut predicates = syn::punctuated::Punctuated::new();
            std::mem::swap(&mut predicates, &mut where_clause.predicates);

            for predicate in predicates.into_iter() {
                match &predicate {
                    syn::WherePredicate::Type(pred) => {
                        if let Some(deps_ident) = &deps_ident {
                            if !is_type_eq_ident(&pred.bounded_ty, deps_ident) {
                                where_clause.predicates.push(predicate);
                            }
                        } else {
                            where_clause.predicates.push(predicate);
                        }
                    }
                    _ => {
                        where_clause.predicates.push(predicate);
                    }
                }
            }
        }
    }
}

fn is_type_eq_ident(ty: &syn::Type, ident: &syn::Ident) -> bool {
    match ty {
        syn::Type::Path(type_path) if type_path.path.segments.len() == 1 => {
            type_path.path.segments.first().unwrap().ident == *ident
        }
        _ => false,
    }
}

fn output_type_tokens(return_type: &syn::ReturnType) -> TokenStream {
    match return_type {
        syn::ReturnType::Default => quote! { () },
        syn::ReturnType::Type(_, ty) => quote! { #ty },
    }
}

fn tidy_generics(generics: &mut syn::Generics) {
    if generics
        .where_clause
        .as_ref()
        .map(|cl| cl.predicates.is_empty())
        .unwrap_or(false)
    {
        generics.where_clause = None;
    }

    if generics.params.is_empty() {
        generics.lt_token = None;
        generics.gt_token = None;
    }
}
