//! When editing generated code in this module, use `__`-prefixed reserved names
//! for macro-emitted local bindings and helper items to avoid collisions with
//! user-defined items at the expansion site.

extern crate core;
extern crate proc_macro;

use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned, ToTokens};
use syn::punctuated::Pair;
use syn::spanned::Spanned;
use syn::{LitStr, Token};

use crate::sym;
use crate::util::{check_duplicate, match_meta};

pub(crate) struct SatsType<'a> {
    pub ident: &'a syn::Ident,
    pub generics: &'a syn::Generics,
    pub name: LitStr,
    pub krate: TokenStream,
    // may want to use in the future
    #[allow(unused)]
    pub original_attrs: &'a [syn::Attribute],
    pub data: SatsTypeData<'a>,
    /// Was the type marked as `#[repr(C)]`?
    pub is_repr_c: bool,
    pub bsatn_pod: bool,
}

pub(crate) enum SatsTypeData<'a> {
    Product(Vec<SatsField<'a>>),
    Sum(Vec<SatsVariant<'a>>),
}

#[derive(Clone)]
pub(crate) struct SatsField<'a> {
    pub ident: Option<&'a syn::Ident>,
    pub vis: &'a syn::Visibility,
    pub name: Option<String>,
    pub ty: &'a syn::Type,
    pub original_attrs: &'a [syn::Attribute],
}

pub(crate) struct SatsVariant<'a> {
    pub ident: &'a syn::Ident,
    pub name: String,
    pub ty: Option<&'a syn::Type>,
    pub member: Option<syn::Member>,
    // may want to use in the future
    #[allow(unused)]
    pub original_attrs: &'a [syn::Attribute],
}

pub(crate) fn sats_type_from_derive(
    input: &syn::DeriveInput,
    crate_fallback: TokenStream,
) -> syn::Result<SatsType<'_>> {
    let data = match &input.data {
        syn::Data::Struct(struc) => {
            let fields = struc.fields.iter().map(|field| SatsField {
                ident: field.ident.as_ref(),
                vis: &field.vis,
                name: field.ident.as_ref().map(syn::Ident::to_string),
                ty: &field.ty,
                original_attrs: &field.attrs,
            });
            SatsTypeData::Product(fields.collect())
        }
        syn::Data::Enum(enu) => {
            let variants = enu.variants.iter().map(|var| {
                let (member, ty) = variant_data(var)?.unzip();
                Ok(SatsVariant {
                    ident: &var.ident,
                    name: var.ident.to_string(),
                    ty,
                    member,
                    original_attrs: &var.attrs,
                })
            });
            SatsTypeData::Sum(variants.collect::<syn::Result<Vec<_>>>()?)
        }
        syn::Data::Union(u) => return Err(syn::Error::new(u.union_token.span, "unions not supported")),
    };
    extract_sats_type(&input.ident, &input.generics, &input.attrs, data, crate_fallback)
}

fn is_repr_c(attrs: &[syn::Attribute]) -> bool {
    let mut is_repr_c = false;
    for attr in attrs.iter().filter(|a| a.path() == sym::repr) {
        let _ = attr.parse_nested_meta(|meta| {
            is_repr_c |= meta.path.is_ident("C");
            Ok(())
        });
    }
    is_repr_c
}

pub(crate) fn extract_sats_type<'a>(
    ident: &'a syn::Ident,
    generics: &'a syn::Generics,
    attrs: &'a [syn::Attribute],
    data: SatsTypeData<'a>,
    crate_fallback: TokenStream,
) -> syn::Result<SatsType<'a>> {
    let mut name = None;
    let mut krate = None;
    let mut bsatn_pod = None;
    for attr in attrs {
        if attr.path() != sym::sats {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            match_meta!(match meta {
                sym::bsatn_pod => {
                    check_duplicate(&bsatn_pod, &meta)?;
                    bsatn_pod = Some(meta.path.span());
                }
                sym::crate_ => {
                    check_duplicate(&krate, &meta)?;
                    let value = meta.value()?;
                    let v = value.call(syn::Path::parse_mod_style)?;
                    krate = Some(v.into_token_stream());
                }
                sym::name => {
                    check_duplicate(&name, &meta)?;
                    let value = meta.value()?;
                    let v = value.parse::<LitStr>()?;
                    name = Some(v);
                }
            });
            Ok(())
        })?;
    }
    let krate = krate.unwrap_or(crate_fallback);
    let name = name.unwrap_or_else(|| crate::util::ident_to_litstr(ident));

    let is_repr_c = is_repr_c(attrs);

    let ty = SatsType {
        ident,
        generics,
        name,
        krate,
        original_attrs: attrs,
        data,
        is_repr_c,
        bsatn_pod: bsatn_pod.is_some(),
    };
    if let Some(span) = bsatn_pod
        && extract_repr_c_primitive(&ty).is_none()
    {
        return Err(syn::Error::new(
            span,
            "bsatn_pod requires a nonempty repr(C) product of fixed-width numeric primitives",
        ));
    }
    Ok(ty)
}

pub(crate) fn derive_satstype(ty: &SatsType<'_>) -> TokenStream {
    let ty_name = &ty.name;
    let name = &ty.ident;
    let krate = &ty.krate;

    let mut add_impls_for_plain_enum = false;
    let typ = match &ty.data {
        SatsTypeData::Product(fields) => {
            let fields = fields.iter().map(|field| {
                let field_name = match &field.name {
                    Some(name) => quote!(Some(#name)),
                    None => quote!(None),
                };
                let ty = field.ty;
                quote!((
                    #field_name,
                    <#ty as #krate::SpacetimeType>::make_type(__typespace)
                ))
            });
            let len = fields.len();
            quote!(
                #krate::sats::AlgebraicType::product::<
                    [(Option<&str>, #krate::sats::AlgebraicType); #len]
                >(
                    [#(#fields),*]
                )
            )
        }
        SatsTypeData::Sum(variants) => {
            // To allow an enum, with all-unit variants, as an index key type,
            // add derive `Filterable` for the enum.
            add_impls_for_plain_enum = variants.iter().all(|var| var.ty.is_none());

            let unit = syn::Type::Tuple(syn::TypeTuple {
                paren_token: Default::default(),
                elems: Default::default(),
            });
            let variants = variants.iter().map(|var| {
                let variant_name = &var.name;
                let ty = var.ty.unwrap_or(&unit);
                quote!((
                    #variant_name,
                    <#ty as #krate::SpacetimeType>::make_type(__typespace)
                ))
            });
            let len = variants.len();
            quote!(
                #krate::sats::AlgebraicType::sum::<
                    [(&str, #krate::sats::AlgebraicType); #len]
                >(
                    [#(#variants),*]
                )
            )
        }
    };

    let mut sats_generics = ty.generics.clone();
    // the 'static here is an unfortunate restriction from TypeId :(
    add_type_bounds(&mut sats_generics, &quote!(#krate::SpacetimeType + 'static));
    let (impl_generics, ty_generics, where_clause) = sats_generics.split_for_impl();

    // TypeId::of() requires all the lifetimes to be 'static
    let mut typeid_generics = sats_generics.clone();
    let static_lt = syn::Lifetime::new("'static", Span::call_site());
    for param in &mut typeid_generics.params {
        if let syn::GenericParam::Lifetime(param) = param {
            param.lifetime = static_lt.clone();
        }
    }
    let (_, typeid_ty_generics, _) = typeid_generics.split_for_impl();

    let impl_plain_enum_extras = if add_impls_for_plain_enum {
        // These will mostly be empty as lifetime and type parameters must be constrained
        // but const parameters don't require constraining.
        let mut generics = ty.generics.clone();
        add_type_bounds(&mut generics, &quote!(#krate::FilterableValue));
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        // Assume that the type is `Copy`, as most all-unit enums will be.
        let filterable_impl = quote! {
            #[automatically_derived]
            impl #impl_generics #krate::Private for #name #ty_generics #where_clause {}
                #[automatically_derived]
            impl #impl_generics #krate::FilterableValue for #name #ty_generics #where_clause {
                type Column = #name #ty_generics;
            }
            #[automatically_derived]
            impl #impl_generics #krate::Private for &#name #ty_generics #where_clause {}
            #[automatically_derived]
            impl #impl_generics #krate::FilterableValue for &#name #ty_generics #where_clause {
                type Column = #name #ty_generics;
            }
        };

        let mut generics = ty.generics.clone();
        add_type_bounds(&mut generics, &quote!(#krate::DirectIndexKey));
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let dik_impl = quote! {
            #[automatically_derived]
            impl #impl_generics #krate::DirectIndexKey for #name #ty_generics #where_clause {}
        };

        Some(quote! {
            #filterable_impl
            #dik_impl
        })
    } else {
        None
    };

    quote! {
        #impl_plain_enum_extras

        #[automatically_derived]
        impl #impl_generics #krate::SpacetimeType for #name #ty_generics #where_clause {
            // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
            fn make_type<S: #krate::sats::typespace::TypespaceBuilder>(__typespace: &mut S) -> #krate::sats::AlgebraicType {
                #krate::sats::typespace::TypespaceBuilder::add(
                    __typespace,
                    core::any::TypeId::of::<#name #typeid_ty_generics>(),
                    Some(#ty_name),
                    // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                    |__typespace| #typ,
                )
            }
        }
    }
}

fn add_type_bounds(generics: &mut syn::Generics, trait_bound: &TokenStream) {
    for param in &generics.params {
        let syn::GenericParam::Type(param) = param else {
            continue;
        };
        let param_name = &param.ident;
        let where_clause = generics.where_clause.get_or_insert_with(|| syn::WhereClause {
            where_token: Default::default(),
            predicates: Default::default(),
        });
        where_clause
            .predicates
            .push(syn::parse_quote!(#param_name: #trait_bound));
    }
}

/// Returns the list of types if syntactically we see that the `ty`
/// is `#[repr(C)]` of only primitives.
///
/// We later assert semantically in generated code that the list of types
/// actually are primitives.
/// We'll also check that `ty` is paddingless.
fn extract_repr_c_primitive<'a>(ty: &'a SatsType) -> Option<Vec<&'a syn::Ident>> {
    // Ensure we have a `#[repr(C)]` struct.
    if !ty.is_repr_c {
        return None;
    }
    let SatsTypeData::Product(fields) = &ty.data else {
        return None;
    };

    if fields.is_empty() {
        return None;
    }
    fields.iter().map(|f| wire_scalar(f).map(|(ident, _)| ident)).collect()
}

/// The wire width and the compile-time proof that a row can be copied verbatim.
fn packed_bsatn_layout(ty: &SatsType) -> Option<(TokenStream, TokenStream)> {
    let fields = extract_repr_c_primitive(ty)?;
    let krate = &ty.krate;
    let width = quote! { #(core::mem::size_of::<#fields>())+* };
    let can_copy = quote! { const {
        #(#krate::bsatn::assert_is_primitive_type::<#fields>();)*
        cfg!(target_endian = "little") && core::mem::size_of::<Self>() == #width
    } };
    Some((width, can_copy))
}

/// Only scalars for which every bit pattern is a valid value.
fn wire_scalar<'a>(field: &SatsField<'a>) -> Option<(&'a syn::Ident, usize)> {
    const SCALARS: &[(sym::Symbol, usize)] = &[
        (sym::u8, 1),
        (sym::i8, 1),
        (sym::u16, 2),
        (sym::i16, 2),
        (sym::u32, 4),
        (sym::i32, 4),
        (sym::f32, 4),
        (sym::u64, 8),
        (sym::i64, 8),
        (sym::f64, 8),
        (sym::u128, 16),
        (sym::i128, 16),
    ];
    let syn::Type::Path(path) = field.ty else { return None };
    if path.qself.is_some() {
        return None;
    }
    let ident = path.path.get_ident()?;
    let (_, width) = SCALARS.iter().find(|(symbol, _)| ident == symbol)?;
    Some((ident, *width))
}

fn scalar_check(scalars: &[&syn::Ident]) -> TokenStream {
    quote! { true #(&& ::core::any::TypeId::of::<#scalars>() == ::core::any::TypeId::of::<::core::primitive::#scalars>())* }
}

const MAX_SCALAR_BLOCK: usize = 128;

/// A complete product small enough for one stack-resident wire block.
fn scalar_row(ty: &SatsType, fields: &[SatsField<'_>]) -> Option<(usize, TokenStream)> {
    if !ty.generics.params.is_empty() || fields.is_empty() {
        return None;
    }
    let scalars = fields.iter().map(wire_scalar).collect::<Option<Vec<_>>>()?;
    let (types, widths): (Vec<_>, Vec<_>) = scalars.into_iter().unzip();
    let width = widths.iter().sum();
    (width <= MAX_SCALAR_BLOCK).then(|| (width, scalar_check(&types)))
}

/// Coalesce consecutive scalar fields into one checked span or one write. This
/// doesn't depend on Rust layout. The type checks fold away for concrete types
/// and keep the ordinary field codec if an identifier shadows a primitive.
fn bsatn_product_fields(ty: &SatsType, fields: &[SatsField<'_>], reading: bool) -> TokenStream {
    let krate = &ty.krate;
    let names: Vec<_> = fields.iter().map(|f| f.ident.unwrap()).collect();
    let slots: Vec<_> = (0..fields.len()).map(|i| quote::format_ident!("__field_{i}")).collect();
    let mut body = TokenStream::new();
    let mut i = 0;
    while i < fields.len() {
        let mut scalars = Vec::new();
        let mut width = 0usize;
        if ty.generics.params.is_empty() {
            for (ident, n) in fields[i..].iter().map_while(wire_scalar) {
                if width + n > MAX_SCALAR_BLOCK {
                    break;
                }
                scalars.push(ident);
                width += n;
            }
        }
        if scalars.len() < 2 {
            let (slot, name) = (&slots[i], names[i]);
            body.extend(if reading {
                quote!(let #slot = __de.field()?;)
            } else {
                quote!(__ser.field(&self.#name)?;)
            });
            i += 1;
            continue;
        }
        let end = i + scalars.len();
        let names = &names[i..end];
        let slots = &slots[i..end];
        let check = scalar_check(&scalars);
        body.extend(if reading {
            quote! {
                let __span = if #check { __de.try_get_array::<#width>() } else { None };
                let (#(#slots,)*) = if let Some(__span) = __span {
                    let mut __src = __span.as_slice();
                    (#(#krate::bsatn::from_reader::<#scalars>(&mut __src)?,)*)
                } else {
                    (#(__de.field::<#scalars>()?,)*)
                };
            }
        } else {
            quote! {
                if #check {
                    let mut __bytes = [0u8; #width];
                    let mut __dst = __bytes.as_mut_slice();
                    #(#krate::bsatn::to_writer(&mut __dst, &self.#names)?;)*
                    __ser.write_field_bytes(&__bytes);
                } else {
                    #(__ser.field(&self.#names)?;)*
                }
            }
        });
        i = end;
    }
    body.extend(if reading {
        quote!(Ok(Self { #(#names: #slots,)* }))
    } else {
        quote!(Ok(()))
    });
    body
}

pub(crate) fn derive_deserialize(ty: &SatsType<'_>) -> TokenStream {
    let (name, tuple_name) = (&ty.ident, &ty.name);
    let spacetimedb_lib = &ty.krate;
    let (impl_generics, ty_generics, where_clause) = ty.generics.split_for_impl();

    let de_lifetime = syn::Lifetime::new("'de", Span::call_site());
    let deserialize_t = quote!(#spacetimedb_lib::de::Deserialize<#de_lifetime>);

    let mut de_generics = ty.generics.clone();

    add_type_bounds(&mut de_generics, &deserialize_t);

    for lp in de_generics.lifetimes_mut() {
        lp.bounds.push(de_lifetime.clone());
    }

    let mut de_lt_param = syn::LifetimeParam::new(de_lifetime);
    de_lt_param.bounds = de_generics
        .lifetimes()
        .map(|lp| Pair::Punctuated(lp.lifetime.clone(), Token![+](Span::call_site())))
        .collect();

    de_generics.params.insert(0, de_lt_param.into());
    let (de_impl_generics, _, de_where_clause) = de_generics.split_for_impl();

    let (iter_n, iter_n2, iter_n3, iter_n4, iter_n5, iter_n6, iter_n7) =
        (0usize.., 0usize.., 0usize.., 0usize.., 0usize.., 0usize.., 0usize..);

    match &ty.data {
        SatsTypeData::Product(fields) => {
            let fast_body = if let Some(types) = extract_repr_c_primitive(ty) {
                let width = quote! { #(core::mem::size_of::<#types>())+* };
                let names = fields.iter().map(|f| f.ident.unwrap());
                // Read one checked wire span; neither input alignment nor Rust struct padding matters.
                let read = quote! {{
                    const { #(#spacetimedb_lib::bsatn::assert_is_primitive_type::<#types>();)* }
                    let mut __end = 0;
                    Self { #(#names: {
                        let __start = __end;
                        __end += core::mem::size_of::<#types>();
                        #types::from_le_bytes(::core::convert::TryInto::try_into(&__bytes[__start..__end]).unwrap())
                    },)* }
                }};
                let decode_vec = if ty.bsatn_pod {
                    quote! {
                        if cfg!(target_endian = "little") {
                            return de.deserialize_pod_vec::<Self>();
                        }
                    }
                } else {
                    TokenStream::new()
                };
                quote! {
                    #[inline(always)]
                    fn deserialize_from_bsatn<R: #spacetimedb_lib::buffer::BufReader<'de>>(
                        mut deserializer: #spacetimedb_lib::bsatn::Deserializer<'_, R>
                    ) -> Result<Self, #spacetimedb_lib::bsatn::DecodeError> {
                        let __bytes = deserializer.get_array::<{ #width }>()?;
                        Ok(#read)
                    }

                    #[inline(always)]
                    fn __deserialize_vec<D: #spacetimedb_lib::de::Deserializer<'de>>(de: D) -> Result<::std::vec::Vec<Self>, D::Error> {
                        #decode_vec
                        de.deserialize_fixed_vec(|__bytes: [u8; #width]| #read)
                    }
                }
            } else {
                let read = bsatn_product_fields(ty, fields, true);
                let bulk = scalar_row(ty, fields).map(|(width, check)| {
                    let names = fields.iter().map(|f| f.ident.unwrap());
                    // Each numeric field accepts every bit pattern. The fixed-vector
                    // decoder supplies exactly the sum of their wire widths.
                    quote! {
                        #[inline]
                        fn __deserialize_vec<__D: #spacetimedb_lib::de::Deserializer<'de>>(__de: __D) -> Result<::std::vec::Vec<Self>, __D::Error> {
                            if #check {
                                __de.deserialize_fixed_vec(|__bytes: [u8; #width]| {
                                    let mut __src = __bytes.as_slice();
                                    Self { #(#names: #spacetimedb_lib::bsatn::from_reader(&mut __src).expect("complete scalar span"),)* }
                                })
                            } else {
                                __de.deserialize_vec()
                            }
                        }
                    }
                });
                quote! {
                    #bulk
                    #[inline]
                    fn deserialize_from_bsatn<__R: #spacetimedb_lib::buffer::BufReader<'de>>(
                        mut __de: #spacetimedb_lib::bsatn::Deserializer<'_, __R>
                    ) -> Result<Self, #spacetimedb_lib::bsatn::DecodeError> {
                        #read
                    }
                }
            };

            let n_fields = fields.len();

            let field_names = fields.iter().map(|f| f.ident.unwrap()).collect::<Vec<_>>();
            let field_strings = fields.iter().map(|f| f.name.as_deref().unwrap()).collect::<Vec<_>>();
            let field_types = fields.iter().map(|f| f.ty);
            let field_types2 = field_types.clone();
            let field_types3 = field_types.clone();
            let field_types4 = field_types.clone();
            quote! {
                #[allow(non_camel_case_types)]
                #[allow(clippy::all)]
                const _: () = {
                    impl #de_impl_generics #spacetimedb_lib::de::Deserialize<'de> for #name #ty_generics #de_where_clause {
                        #fast_body

                        fn deserialize<D: #spacetimedb_lib::de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                            deserializer.deserialize_product(__ProductVisitor {
                                _marker: std::marker::PhantomData::<fn() -> #name #ty_generics>,
                            })
                        }

                        fn validate<D: #spacetimedb_lib::de::Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
                            deserializer.validate_product(__ProductVisitor {
                                _marker: std::marker::PhantomData::<fn() -> #name #ty_generics>,
                            })
                        }
                    }

                    // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                    struct __ProductVisitor #impl_generics #where_clause {
                        _marker: std::marker::PhantomData<fn() -> #name #ty_generics>,
                    }

                    impl #de_impl_generics #spacetimedb_lib::de::ProductVisitor<'de> for __ProductVisitor #ty_generics #de_where_clause {
                        type Output = #name #ty_generics;

                        fn product_name(&self) -> Option<&str> {
                            Some(#tuple_name)
                        }
                        fn product_len(&self) -> usize {
                            #n_fields
                        }

                        #[inline]
                        fn visit_seq_product<A: #spacetimedb_lib::de::SeqProductAccess<'de>>(self, mut tup: A) -> Result<Self::Output, A::Error> {
                            Ok(#name {
                                #(#field_names:
                                    tup.next_element::<#field_types>()?
                                        .ok_or_else(|| #spacetimedb_lib::de::Error::invalid_product_length(#iter_n, &self))?,)*
                            })
                        }
                        fn validate_seq_product<A: #spacetimedb_lib::de::SeqProductAccess<'de>>(self, mut tup: A) -> Result<(), A::Error> {
                            #(
                                tup.validate_next_element::<#field_types2>()?
                                    .ok_or_else(|| #spacetimedb_lib::de::Error::invalid_product_length(#iter_n2, &self))?;
                            )*
                            Ok(())
                        }
                        // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                        fn visit_named_product<A: #spacetimedb_lib::de::NamedProductAccess<'de>>(self, mut __prod: A) -> Result<Self::Output, A::Error> {
                            #(let mut #field_names = None;)*
                            // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                            while let Some(__field) = #spacetimedb_lib::de::NamedProductAccess::get_field_ident(&mut __prod, Self {
                                _marker: std::marker::PhantomData,
                            })? {
                                match __field {
                                    #(__ProductFieldIdent::#field_names => {
                                        if #field_names.is_some() {
                                            return Err(#spacetimedb_lib::de::Error::duplicate_field(#iter_n3, Some(#field_strings), &self))
                                        }
                                        #field_names = Some(#spacetimedb_lib::de::NamedProductAccess::get_field_value::<#field_types3>(&mut __prod)?)
                                    })*
                                }
                            }
                            Ok(#name {
                                #(#field_names:
                                    #field_names.ok_or_else(|| #spacetimedb_lib::de::Error::missing_field(#iter_n4, Some(#field_strings), &self))?,)*
                            })
                        }
                        // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                        fn validate_named_product<A: #spacetimedb_lib::de::NamedProductAccess<'de>>(self, mut __prod: A) -> Result<(), A::Error> {
                            #(let mut #field_names = false;)*
                            // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                            while let Some(__field) = #spacetimedb_lib::de::NamedProductAccess::get_field_ident(&mut __prod, Self {
                                _marker: std::marker::PhantomData,
                            })? {
                                match __field {
                                    #(__ProductFieldIdent::#field_names => {
                                        if #field_names {
                                            return Err(#spacetimedb_lib::de::Error::duplicate_field(#iter_n5, Some(#field_strings), &self))
                                        }
                                        #spacetimedb_lib::de::NamedProductAccess::validate_field_value::<#field_types4>(&mut __prod)?;
                                        #field_names = true;
                                    })*
                                }
                            }
                            #(
                                if !#field_names {
                                    return Err(#spacetimedb_lib::de::Error::missing_field(#iter_n6, Some(#field_strings), &self));
                                }
                            )*
                            Ok(())
                        }
                    }

                    impl #de_impl_generics #spacetimedb_lib::de::FieldNameVisitor<'de> for __ProductVisitor #ty_generics #de_where_clause {
                        type Output = __ProductFieldIdent;

                        fn field_names(&self) -> impl '_ + Iterator<Item = Option<&str>> {
                            [#(#field_strings),*].into_iter().map(Some)
                        }

                        // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                        // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                        fn visit<__E: #spacetimedb_lib::de::Error>(self, __name: &str) -> Result<Self::Output, __E> {
                            match __name {
                                #(#field_strings => Ok(__ProductFieldIdent::#field_names),)*
                                _ => Err(#spacetimedb_lib::de::Error::unknown_field_name(__name, &self)),
                            }
                        }

                        // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                        fn visit_seq(self, __index: usize) -> Self::Output {
                            match __index {
                                #(#iter_n7 => __ProductFieldIdent::#field_names,)*
                                _ => core::unreachable!(),
                            }
                        }
                    }

                    #[allow(non_camel_case_types)]
                    // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                    enum __ProductFieldIdent {
                        #(#field_names,)*
                    }
                };
            }
        }
        SatsTypeData::Sum(variants) => {
            let variant_names = variants.iter().map(|var| &*var.name).collect::<Vec<_>>();
            let variant_idents = variants.iter().map(|var| var.ident).collect::<Vec<_>>();
            let tags = 0u8..;
            let binary_arms = variants.iter().zip(0u8..).map(|(var, tag)| {
                let ident = var.ident;
                if let Some(member) = &var.member {
                    quote!(#tag => Ok(Self::#ident { #member: __de.field()? }))
                } else {
                    quote!(#tag => Ok(Self::#ident))
                }
            });
            let arms = variants.iter().map(|var| {
                let ident = var.ident;
                if let (Some(member), Some(ty)) = (&var.member, var.ty) {
                    quote! {
                        __Variant::#ident => Ok(#name::#ident { #member: #spacetimedb_lib::de::VariantAccess::deserialize::<#ty>(__access)? }),
                    }
                } else {
                    quote! {
                        __Variant::#ident => {
                            let () = #spacetimedb_lib::de::VariantAccess::deserialize(__access)?;
                            Ok(#name::#ident)
                        }
                    }
                }
            });
            let arms_validate = variants.iter().map(|var| {
                let ident = var.ident;
                if let Some(ty) = var.ty {
                    quote! {
                        __Variant::#ident => #spacetimedb_lib::de::VariantAccess::validate::<#ty>(__access)?,
                    }
                } else {
                    quote! {
                        __Variant::#ident => #spacetimedb_lib::de::VariantAccess::validate::<()>(__access)?,
                    }
                }
            });
            quote! {
                #[allow(clippy::all)]
                const _: () = {
                    impl #de_impl_generics #spacetimedb_lib::de::Deserialize<'de> for #name #ty_generics #de_where_clause {
                        #[inline]
                        fn deserialize_from_bsatn<__R: #spacetimedb_lib::buffer::BufReader<'de>>(
                            mut __de: #spacetimedb_lib::bsatn::Deserializer<'_, __R>
                        ) -> Result<Self, #spacetimedb_lib::bsatn::DecodeError> {
                            match __de.field::<::core::primitive::u8>()? {
                                #(#binary_arms,)*
                                __tag => Err(#spacetimedb_lib::bsatn::DecodeError::InvalidTag {
                                    tag: __tag, sum_name: Some(#tuple_name.to_owned()),
                                }),
                            }
                        }
                        fn deserialize<D: #spacetimedb_lib::de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                            deserializer.deserialize_sum(__SumVisitor {
                                _marker: std::marker::PhantomData::<fn() -> #name #ty_generics>,
                            })
                        }

                        fn validate<D: #spacetimedb_lib::de::Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
                            deserializer.validate_sum(__SumVisitor {
                                _marker: std::marker::PhantomData::<fn() -> #name #ty_generics>,
                            })
                        }
                    }

                    // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                    struct __SumVisitor #impl_generics #where_clause {
                        _marker: std::marker::PhantomData<fn() -> #name #ty_generics>,
                    }

                    impl #de_impl_generics #spacetimedb_lib::de::SumVisitor<'de> for __SumVisitor #ty_generics #de_where_clause {
                        type Output = #name #ty_generics;

                        fn sum_name(&self) -> Option<&str> {
                            Some(#tuple_name)
                        }

                        // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                        fn visit_sum<A: #spacetimedb_lib::de::SumAccess<'de>>(self, __data: A) -> Result<Self::Output, A::Error> {
                            // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                            let (__variant, __access) = __data.variant(self)?;
                            match __variant {
                                #(#arms)*
                            }
                        }

                        // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                        fn validate_sum<A: #spacetimedb_lib::de::SumAccess<'de>>(self, __data: A) -> Result<(), A::Error> {
                            // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                            let (__variant, __access) = __data.variant(self)?;
                            match __variant {
                                #(#arms_validate)*
                            }
                            Ok(())
                        }
                    }

                    #[allow(non_camel_case_types)]
                    // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                    enum __Variant {
                        #(#variant_idents,)*
                    }

                    impl #de_impl_generics #spacetimedb_lib::de::VariantVisitor<'de> for __SumVisitor #ty_generics #de_where_clause {
                        type Output = __Variant;

                        fn variant_names(&self) -> impl '_ + Iterator<Item = &str> {
                            [#(#variant_names,)*].into_iter()
                        }

                        // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                        fn visit_tag<E: #spacetimedb_lib::de::Error>(self, __tag: u8) -> Result<Self::Output, E> {
                            match __tag {
                                #(#tags => Ok(__Variant::#variant_idents),)*
                                _ => Err(#spacetimedb_lib::de::Error::unknown_variant_tag(__tag, &self)),
                            }
                        }
                        // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                        fn visit_name<E: #spacetimedb_lib::de::Error>(self, __name: &str) -> Result<Self::Output, E> {
                            match __name {
                                #(#variant_names => Ok(__Variant::#variant_idents),)*
                                _ => Err(#spacetimedb_lib::de::Error::unknown_variant_name(__name, &self)),
                            }
                        }
                    }
                };
            }
        }
    }
}

pub(crate) fn derive_serialize(ty: &SatsType) -> TokenStream {
    let spacetimedb_lib = &ty.krate;
    let name = &ty.ident;

    let mut generics = ty.generics.clone();

    let serialize_t = quote!(#spacetimedb_lib::ser::Serialize);
    add_type_bounds(&mut generics, &serialize_t);

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let mut fast_body = None;
    let size_hint;
    let body = match &ty.data {
        SatsTypeData::Product(fields) => {
            let names = fields.iter().map(|field| field.ident.unwrap());
            let types = fields.iter().map(|field| field.ty);
            size_hint = Some(quote! {
                const BSATN_FIXED_SIZE: ::core::option::Option<usize> =
                    #spacetimedb_lib::bsatn::product_width(&[#(<#types as #serialize_t>::BSATN_FIXED_SIZE),*]);

                fn bsatn_size_hint(&self) -> ::core::option::Option<usize> {
                    let __size = 0usize;
                    #(let __size = __size.checked_add(#spacetimedb_lib::ser::Serialize::bsatn_size_hint(&self.#names)?)?;)*
                    Some(__size)
                }
            });
            if let Some((width, can_copy)) = packed_bsatn_layout(ty) {
                let names = fields.iter().map(|f| f.ident.unwrap());
                let as_bytes = if ty.bsatn_pod {
                    quote! { #spacetimedb_lib::bsatn::pod_bytes(__items) }
                } else {
                    // SAFETY: `__items` is a valid slice; `can_copy` proves that its
                    // entire footprint is initialized and is precisely its BSATN payload.
                    quote! { unsafe {
                        core::slice::from_raw_parts(__items.as_ptr().cast::<u8>(), core::mem::size_of_val(__items))
                    } }
                };
                fast_body = Some(quote! {
                    #[inline(always)]
                    fn serialize_into_bsatn<W: #spacetimedb_lib::buffer::BufWriter>(
                            &self,
                            serializer: #spacetimedb_lib::bsatn::Serializer<'_, W>
                    ) -> Result<(), #spacetimedb_lib::bsatn::EncodeError> {
                        let mut __bytes = [0u8; #width];
                        let mut __end = 0;
                        #({
                            let __field = self.#names.to_le_bytes();
                            let __start = __end;
                            __end += __field.len();
                            __bytes[__start..__end].copy_from_slice(&__field);
                        })*
                        serializer.raw_write_bytes(&__bytes);
                        Ok(())
                    }

                    fn __serialize_array<S: #spacetimedb_lib::ser::Serializer>(__items: &[Self], ser: S) -> Result<S::Ok, S::Error> {
                        if #can_copy {
                            ser.serialize_packed_array(__items, #as_bytes)
                        } else {
                            ser.serialize_slice(__items)
                        }
                    }
                });
            } else {
                let write = bsatn_product_fields(ty, fields, false);
                let fresh = scalar_row(ty, fields).map(|(width, check)| {
                    let names = fields.iter().map(|f| f.ident.unwrap());
                    let pack = quote! {
                        let mut __bytes = [0u8; #width];
                        let mut __dst = __bytes.as_mut_slice();
                        #(#spacetimedb_lib::bsatn::to_writer(&mut __dst, &__row.#names).expect("scalar write");)*
                        __bytes
                    };
                    quote! {
                        #[inline]
                        fn __bsatn_vec(&self) -> ::core::option::Option<::std::vec::Vec<::core::primitive::u8>> {
                            if !(#check) { return None; }
                            let __row = self;
                            Some({ #pack }.to_vec())
                        }

                        fn __serialize_array<__S: #spacetimedb_lib::ser::Serializer>(__rows: &[Self], __ser: __S) -> Result<__S::Ok, __S::Error> {
                            if #check && __rows.len() >= 8 {
                                __ser.serialize_fixed_array(__rows, |__row| { #pack })
                            } else {
                                __ser.serialize_slice(__rows)
                            }
                        }
                    }
                });
                fast_body = Some(quote! {
                    #fresh
                    #[inline]
                    fn serialize_into_bsatn<__W: #spacetimedb_lib::buffer::BufWriter>(
                        &self, mut __ser: #spacetimedb_lib::bsatn::Serializer<'_, __W>
                    ) -> Result<(), #spacetimedb_lib::bsatn::EncodeError> {
                        #write
                    }
                });
            }

            let fieldnames = fields.iter().map(|field| field.ident.unwrap());
            let tys = fields.iter().map(|f| &f.ty);
            let fieldnamestrings = fields.iter().map(|field| field.name.as_ref().unwrap());
            let nfields = fields.len();
            quote! {
                // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                let mut __prod = __serializer.serialize_named_product(#nfields)?;
                #(#spacetimedb_lib::ser::SerializeNamedProduct::serialize_element::<#tys>(&mut __prod, Some(#fieldnamestrings), &self.#fieldnames)?;)*
                #spacetimedb_lib::ser::SerializeNamedProduct::end(__prod)
            }
        }
        SatsTypeData::Sum(variants) => {
            let arms = variants.iter().enumerate().map(|(i, var)| {
                let (name,name_str) = (var.ident, &var.name);
                let tag = i as u8;
                if let (Some(member), Some(ty)) = (&var.member, var.ty) {
                    quote_spanned! {ty.span()=>
                        // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
                        Self::#name { #member: __variant } => __serializer.serialize_variant::<#ty>(#tag, Some(#name_str), __variant),
                    }
                } else {
                    quote! {
                        Self::#name => __serializer.serialize_variant(#tag, Some(#name_str), &()),
                    }
                }
            });
            let hints = variants.iter().map(|var| {
                let name = var.ident;
                if let Some(member) = &var.member {
                    quote!(Self::#name { #member: __v } => #spacetimedb_lib::ser::Serialize::bsatn_size_hint(__v)?.checked_add(1))
                } else {
                    quote!(Self::#name => Some(1))
                }
            });
            let width = variants.iter().all(|v| v.member.is_none()).then(|| {
                quote! {
                    const BSATN_FIXED_SIZE: ::core::option::Option<usize> = Some(1);
                }
            });
            size_hint = Some(quote! {
                #width
                fn bsatn_size_hint(&self) -> ::core::option::Option<usize> {
                    match self { #(#hints,)* _ => None }
                }
            });
            quote!(match self {
                #(#arms)*
                _ => unreachable!(),
            })
        }
    };
    quote! {
        impl #impl_generics #spacetimedb_lib::ser::Serialize for #name #ty_generics #where_clause {
            #fast_body
            #size_hint
            // __ reserved name for binding to prevent name conflicts. See module-level doc comment.
            fn serialize<S: #spacetimedb_lib::ser::Serializer>(&self, __serializer: S) -> Result<S::Ok, S::Error> {
                #body
            }
        }
    }
}

fn variant_data(variant: &syn::Variant) -> syn::Result<Option<(syn::Member, &syn::Type)>> {
    let field = match &variant.fields {
        syn::Fields::Named(f) if f.named.len() == 1 => &f.named[0],
        syn::Fields::Named(_) => {
            return Err(syn::Error::new_spanned(
                &variant.fields,
                "must be a unit variant or a newtype variant",
            ))
        }
        syn::Fields::Unnamed(f) if f.unnamed.len() != 1 => {
            return Err(syn::Error::new_spanned(
                &variant.fields,
                "must be a unit variant or a newtype variant",
            ))
        }
        syn::Fields::Unnamed(f) => &f.unnamed[0],
        syn::Fields::Unit => return Ok(None),
    };
    let member = field
        .ident
        .clone()
        .map(Into::into)
        .unwrap_or_else(|| syn::Member::from(0));
    Ok(Some((member, &field.ty)))
}
