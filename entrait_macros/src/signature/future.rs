use proc_macro2::Span;
use proc_macro2::TokenStream;
use quote::quote;
use quote::ToTokens;

use crate::generics::TraitIndirection;
use crate::idents::CrateIdents;
use crate::token_util::EmptyToken;
use crate::token_util::Punctuator;

use super::lifetimes;
use super::AssociatedFut;
use super::EntraitSignature;
use super::ReceiverGeneration;
use super::SigComponent;

impl EntraitSignature {
    pub fn convert_to_associated_future(
        &mut self,
        receiver_generation: ReceiverGeneration,
        trait_span: Span,
    ) {
        lifetimes::de_elide_lifetimes(self, receiver_generation);

        let output = clone_output_type(&self.sig.output);

        // make the function generic if it wasn't already
        let sig = &mut self.sig;
        sig.asyncness = None;
        let generics = &mut sig.generics;
        generics.lt_token.get_or_insert(syn::parse_quote! { < });
        generics.gt_token.get_or_insert(syn::parse_quote! { > });

        // insert generated/non-user-provided lifetimes
        for fut_lifetime in self.lifetimes.iter().filter(|lt| !lt.user_provided.0) {
            generics
                .params
                .push(syn::GenericParam::Lifetime(syn::LifetimeDef {
                    attrs: vec![],
                    lifetime: fut_lifetime.lifetime.clone(),
                    colon_token: None,
                    bounds: syn::punctuated::Punctuated::new(),
                }));
        }

        let fut_ident = quote::format_ident!("Fut__{}", sig.ident);

        let fut_lifetimes = self
            .lifetimes
            .iter()
            .map(|ft| &ft.lifetime)
            .collect::<Vec<_>>();

        self.sig.output = syn::parse_quote_spanned! { trait_span =>
            -> Self::#fut_ident<#(#fut_lifetimes),*>
        };

        self.associated_fut = Some(AssociatedFut {
            ident: fut_ident,
            output,
        });
    }
}

fn clone_output_type(return_type: &syn::ReturnType) -> syn::Type {
    match return_type {
        syn::ReturnType::Default => syn::parse_quote! { () },
        syn::ReturnType::Type(_, ty) => ty.as_ref().clone(),
    }
}

pub struct FutDecl<'s> {
    pub signature: &'s EntraitSignature,
    pub associated_fut: &'s AssociatedFut,
    pub trait_indirection: TraitIndirection,
    pub crate_idents: &'s CrateIdents,
}

impl<'s> ToTokens for FutDecl<'s> {
    fn to_tokens(&self, stream: &mut TokenStream) {
        let ident = &self.associated_fut.ident;
        let core = &self.crate_idents.core;
        let output = &self.associated_fut.output;

        let params = FutParams {
            signature: self.signature,
        };
        let where_clause = WhereClause {
            signature: self.signature,
            trait_indirection: self.trait_indirection,
        };

        let tokens = quote! {
            #[allow(non_camel_case_types)]
            type #ident #params: ::#core::future::Future<Output = #output> + Send #where_clause;
        };

        tokens.to_tokens(stream);
    }
}

pub struct FutImpl<'s> {
    pub signature: &'s EntraitSignature,
    pub associated_fut: &'s AssociatedFut,
    pub trait_indirection: TraitIndirection,
    pub crate_idents: &'s CrateIdents,
}

impl<'s> ToTokens for FutImpl<'s> {
    fn to_tokens(&self, stream: &mut TokenStream) {
        let ident = &self.associated_fut.ident;
        let core = &self.crate_idents.core;
        let output = &self.associated_fut.output;

        let params = FutParams {
            signature: self.signature,
        };
        let where_clause = WhereClause {
            signature: self.signature,
            trait_indirection: self.trait_indirection,
        };

        let tokens = quote! {
            #[allow(non_camel_case_types)]
            type #ident #params = impl ::#core::future::Future<Output = #output> #where_clause;
        };
        tokens.to_tokens(stream);
    }
}

struct FutParams<'s> {
    signature: &'s EntraitSignature,
}

impl<'s> ToTokens for FutParams<'s> {
    fn to_tokens(&self, stream: &mut TokenStream) {
        let mut punctuator = Punctuator::new(
            stream,
            syn::token::Lt::default(),
            syn::token::Comma::default(),
            syn::token::Gt::default(),
        );

        for lt in &self.signature.lifetimes {
            punctuator.push(&lt.lifetime);
        }
    }
}

struct WhereClause<'s> {
    signature: &'s EntraitSignature,
    trait_indirection: TraitIndirection,
}

impl<'s> ToTokens for WhereClause<'s> {
    fn to_tokens(&self, stream: &mut TokenStream) {
        let bound_target = match self.trait_indirection {
            TraitIndirection::Static | TraitIndirection::Dynamic => quote! { EntraitT },
            TraitIndirection::None => quote! { Self },
        };

        let mut punctuator = Punctuator::new(
            stream,
            quote! {
                where #bound_target:
            },
            syn::token::Add::default(),
            EmptyToken,
        );

        for lt in self
            .signature
            .lifetimes
            .iter()
            .filter(|ft| ft.source == SigComponent::Receiver)
        {
            punctuator.push(&lt.lifetime);
        }
    }
}
