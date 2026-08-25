use makepad_micro_proc_macro::*;
use proc_macro::TokenStream;

struct JsonField<'a> {
    field: &'a StructField,
    json_name: String,
    key_prefix: String,
    comma_key_prefix: String,
    local_name: String,
    optional: bool,
}

#[inline]
fn get_json_field_name(field: &StructField) -> String {
    for attr in &field.attrs {
        if attr.name == "rename" {
            if let Some(ref args) = attr.args {
                let mut parser = TokenParser::new(args.clone());
                if let Some(name) = parser.eat_any_ident() {
                    return name;
                }
            }
        }
    }

    if let Some(name) = field.name.strip_prefix('_') {
        name.to_string()
    } else {
        field.name.clone()
    }
}

#[inline]
fn is_option_type(field: &StructField) -> bool {
    let mut ty = String::new();
    for token in field.ty.clone() {
        ty.push_str(&token.to_string());
    }
    ty.retain(|c| !c.is_whitespace());

    ty == "Option"
        || ty.starts_with("Option<")
        || ty == "std::option::Option"
        || ty.starts_with("std::option::Option<")
        || ty == "core::option::Option"
        || ty.starts_with("core::option::Option<")
        || ty == "::std::option::Option"
        || ty.starts_with("::std::option::Option<")
        || ty == "::core::option::Option"
        || ty.starts_with("::core::option::Option<")
}

#[inline]
fn collect_json_fields<'a>(fields: &'a [StructField]) -> Vec<JsonField<'a>> {
    fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let json_name = get_json_field_name(field);
            JsonField {
                field,
                key_prefix: json_key_prefix(&json_name, false),
                comma_key_prefix: json_key_prefix(&json_name, true),
                json_name,
                local_name: format!("__makepad_json_field_{}", index),
                optional: is_option_type(field),
            }
        })
        .collect()
}

#[inline]
fn json_key_prefix(name: &str, comma: bool) -> String {
    let mut out = String::with_capacity(name.len() + if comma { 4 } else { 3 });
    if comma {
        out.push(',');
    }
    out.push('"');
    out.push_str(name);
    out.push_str("\":");
    out
}

#[inline]
fn enum_open_prefix(variant: &str, payload_open: char) -> String {
    let mut out = String::with_capacity(variant.len() + 6);
    out.push_str("{\"");
    out.push_str(variant);
    out.push_str("\":");
    out.push(payload_open);
    out
}

#[inline]
fn bare_enum_json(variant: &str) -> String {
    let mut out = String::with_capacity(variant.len() + 8);
    out.push_str("{\"");
    out.push_str(variant);
    out.push_str("\":[]}");
    out
}

#[inline]
fn add_push_str(tb: &mut TokenBuilder, value: &str) {
    if value.len() == 1 && value.as_bytes()[0].is_ascii() {
        tb.add("s . out . push (")
            .chr(value.as_bytes()[0] as char)
            .add(") ;");
    } else {
        tb.add("s . out . push_str (").string(value).add(") ;");
    }
}

#[derive(Clone, Copy)]
enum SerFieldSource {
    Struct,
    Binding,
}

#[inline]
fn add_optional_open(tb: &mut TokenBuilder, field: &JsonField<'_>, source: SerFieldSource) {
    tb.add("if let Some ( __makepad_json_value ) = ");
    match source {
        SerFieldSource::Struct => {
            tb.add("& self .").ident(&field.field.name);
        }
        SerFieldSource::Binding => {
            tb.ident(&field.local_name);
        }
    }
    tb.add("{");
}

#[inline]
fn add_required_ser(
    tb: &mut TokenBuilder,
    field: &JsonField<'_>,
    source: SerFieldSource,
) {
    match source {
        SerFieldSource::Struct => {
            tb.add("self .").ident(&field.field.name);
        }
        SerFieldSource::Binding => {
            tb.ident(&field.local_name);
        }
    }
    tb.add(". ser_json ( d + 1 , s ) ;");
}

#[inline]
fn add_optional_ser(tb: &mut TokenBuilder) {
    tb.add("__makepad_json_value . ser_json ( d + 1 , s ) ;");
}

fn add_named_ser(
    tb: &mut TokenBuilder,
    fields: &[JsonField<'_>],
    source: SerFieldSource,
    open: &str,
    close: &str,
) {
    if fields.is_empty() {
        let mut complete = String::with_capacity(open.len() + close.len());
        complete.push_str(open);
        complete.push_str(close);
        add_push_str(tb, &complete);
        return;
    }

    let first_required = fields.iter().position(|field| !field.optional);

    // Once a required field is emitted, every following field has a guaranteed
    // predecessor and can use a compile-time comma prefix with no first-field
    // branch. This is the common and fastest path.
    if first_required == Some(0) {
        let mut first = String::with_capacity(open.len() + fields[0].json_name.len() + 3);
        first.push_str(open);
        first.push_str(&fields[0].key_prefix);
        add_push_str(tb, &first);
        add_required_ser(tb, &fields[0], source);

        for field in &fields[1..] {
            if field.optional {
                add_optional_open(tb, field, source);
                add_push_str(tb, &field.comma_key_prefix);
                add_optional_ser(tb);
                tb.add("}");
            } else {
                add_push_str(tb, &field.comma_key_prefix);
                add_required_ser(tb, field, source);
            }
        }

        add_push_str(tb, close);
        return;
    }

    add_push_str(tb, open);
    tb.add("let mut __makepad_json_first = true ;");

    let optional_prefix_len = first_required.unwrap_or(fields.len());
    for (index, field) in fields[..optional_prefix_len].iter().enumerate() {
        debug_assert!(field.optional);
        add_optional_open(tb, field, source);

        if index == 0 {
            tb.add("__makepad_json_first = false ;");
            add_push_str(tb, &field.key_prefix);
        } else {
            tb.add("if __makepad_json_first { __makepad_json_first = false ;");
            add_push_str(tb, &field.key_prefix);
            tb.add("} else {");
            add_push_str(tb, &field.comma_key_prefix);
            tb.add("}");
        }

        add_optional_ser(tb);
        tb.add("}");
    }

    if let Some(required_index) = first_required {
        let required = &fields[required_index];
        tb.add("if __makepad_json_first {");
        add_push_str(tb, &required.key_prefix);
        tb.add("} else {");
        add_push_str(tb, &required.comma_key_prefix);
        tb.add("}");
        add_required_ser(tb, required, source);

        for field in &fields[required_index + 1..] {
            if field.optional {
                add_optional_open(tb, field, source);
                add_push_str(tb, &field.comma_key_prefix);
                add_optional_ser(tb);
                tb.add("}");
            } else {
                add_push_str(tb, &field.comma_key_prefix);
                add_required_ser(tb, field, source);
            }
        }
    } else {
        // Keep the final assignment observably used so warning-as-error builds
        // do not reject an all-optional structure.
        tb.add("let _ = __makepad_json_first ;");
    }

    add_push_str(tb, close);
}

fn add_tuple_struct_ser(tb: &mut TokenBuilder, count: usize) {
    if count == 0 {
        add_push_str(tb, "[]");
        return;
    }

    tb.add("s . out . push (").chr('[').add(") ;");
    for index in 0..count {
        if index != 0 {
            tb.add("s . out . push (").chr(',').add(") ;");
        }
        tb.add("self .")
            .unsuf_usize(index)
            .add(". ser_json ( d , s ) ;");
    }
    tb.add("s . out . push (").chr(']').add(") ;");
}

fn add_tuple_variant_ser(
    tb: &mut TokenBuilder,
    variant: &str,
    bindings: &[String],
) {
    add_push_str(tb, &enum_open_prefix(variant, '['));
    for (index, binding) in bindings.iter().enumerate() {
        if index != 0 {
            tb.add("s . out . push (").chr(',').add(") ;");
        }
        tb.ident(binding).add(". ser_json ( d , s ) ;");
    }
    add_push_str(tb, "]}");
}

fn add_named_de_parse(tb: &mut TokenBuilder, fields: &[JsonField<'_>]) {
    tb.add("s . curly_open ( i ) ? ;");

    for field in fields {
        tb.add("let mut ").ident(&field.local_name).add("= None ;");
    }

    tb.add("while let Some ( _ ) = s . next_str ( ) {");
    tb.add("match s . strbuf . as_str ( ) {");

    for field in fields {
        tb.string(&field.json_name)
            .add("=> { s . next_colon ( i ) ? ;");
        tb.ident(&field.local_name)
            .add("= Some ( DeJson :: de_json ( s , i ) ? ) ; } ,");
    }

    tb.add("_ => { if s . lenient { s . next_colon ( i ) ? ; s . skip_value ( i ) ? ; } else { return std :: result :: Result :: Err ( s . err_exp ( & s . strbuf ) ) ; } }");
    tb.add("} ; s . eat_comma_curly ( i ) ? ; }");
    tb.add("s . curly_close ( i ) ? ;");
}

fn add_named_de_values(tb: &mut TokenBuilder, fields: &[JsonField<'_>]) {
    for field in fields {
        tb.ident(&field.field.name).add(":");
        if field.optional {
            tb.add("if let Some ( __makepad_json_value ) = ")
                .ident(&field.local_name)
                .add("{ __makepad_json_value } else { None } ,");
        } else {
            tb.add("if let Some ( __makepad_json_value ) = ")
                .ident(&field.local_name)
                .add("{ __makepad_json_value } else { return std :: result :: Result :: Err ( s . err_nf (");
            tb.string(&field.json_name).add(") ) ; } ,");
        }
    }
}

fn add_tuple_de_values(tb: &mut TokenBuilder, count: usize) {
    for _ in 0..count {
        tb.add("{ let __makepad_json_value = DeJson :: de_json ( s , i ) ? ; s . eat_comma_block ( i ) ? ; __makepad_json_value } ,");
    }
}

pub fn derive_ser_json_impl(input: TokenStream) -> TokenStream {
    let mut parser = TokenParser::new(input);
    let mut tb = TokenBuilder::new();

    parser.eat_attributes();
    parser.eat_ident("pub");

    if parser.eat_ident("struct") {
        if let Some(name) = parser.eat_any_ident() {
            let generic = parser.eat_generic();
            let types = parser.eat_all_types();
            let where_clause = parser.eat_where_clause(Some("SerJson"));

            tb.add("impl").stream(generic.clone());
            tb.add("SerJson for")
                .ident(&name)
                .stream(generic)
                .stream(where_clause);
            tb.add("{ # [ inline ] fn ser_json ( & self , d : usize , s : & mut SerJsonState ) {");

            if let Some(types) = types {
                add_tuple_struct_ser(&mut tb, types.len());
            } else if let Some(fields) = parser.eat_all_struct_fields() {
                let fields = collect_json_fields(&fields);
                add_named_ser(&mut tb, &fields, SerFieldSource::Struct, "{", "}");
            } else {
                return parser.unexpected();
            }

            tb.add("} } ;");
            return tb.end();
        }
    } else if parser.eat_ident("enum") {
        if let Some(name) = parser.eat_any_ident() {
            let generic = parser.eat_generic();
            let where_clause = parser.eat_where_clause(Some("SerJson"));

            tb.add("impl").stream(generic.clone());
            tb.add("SerJson for")
                .ident(&name)
                .stream(generic)
                .stream(where_clause);
            tb.add("{ # [ inline ] fn ser_json ( & self , d : usize , s : & mut SerJsonState ) { match self {");

            if !parser.open_brace() {
                return parser.unexpected();
            }

            while !parser.eat_eot() {
                parser.eat_attributes();
                if let Some(variant) = parser.eat_any_ident() {
                    if let Some(types) = parser.eat_all_types() {
                        let bindings: Vec<String> = (0..types.len())
                            .map(|index| format!("__makepad_json_variant_{}", index))
                            .collect();

                        tb.add("Self ::").ident(&variant).add("(");
                        for binding in &bindings {
                            tb.ident(binding).add(",");
                        }
                        tb.add(") => {");
                        add_tuple_variant_ser(&mut tb, &variant, &bindings);
                        tb.add("}");
                    } else if let Some(fields) = parser.eat_all_struct_fields() {
                        let fields = collect_json_fields(&fields);

                        tb.add("Self ::").ident(&variant).add("{");
                        for field in &fields {
                            tb.ident(&field.field.name)
                                .add(":")
                                .ident(&field.local_name)
                                .add(",");
                        }
                        tb.add("} => {");

                        let open = enum_open_prefix(&variant, '{');
                        add_named_ser(
                            &mut tb,
                            &fields,
                            SerFieldSource::Binding,
                            &open,
                            "}}",
                        );
                        tb.add("}");
                    } else if parser.is_punct_alone(',') || parser.is_eot() {
                        tb.add("Self ::").ident(&variant).add("=> {");
                        add_push_str(&mut tb, &bare_enum_json(&variant));
                        tb.add("}");
                    } else {
                        return parser.unexpected();
                    }
                    parser.eat_punct_alone(',');
                } else {
                    return parser.unexpected();
                }
            }

            tb.add("} } } ;");
            return tb.end();
        }
    }

    parser.unexpected()
}

pub fn derive_de_json_impl(input: TokenStream) -> TokenStream {
    let mut parser = TokenParser::new(input);
    let mut tb = TokenBuilder::new();

    parser.eat_attributes();
    parser.eat_ident("pub");

    if parser.eat_ident("struct") {
        if let Some(name) = parser.eat_any_ident() {
            let generic = parser.eat_generic();
            let types = parser.eat_all_types();
            let where_clause = parser.eat_where_clause(Some("DeJson"));

            tb.add("impl").stream(generic.clone());
            tb.add("DeJson for")
                .ident(&name)
                .stream(generic)
                .stream(where_clause);
            tb.add("{ # [ inline ] fn de_json ( s : & mut DeJsonState , i : & mut std :: str :: Chars )");
            tb.add("-> std :: result :: Result < Self , DeJsonErr > {");

            if let Some(types) = types {
                tb.add("s . block_open ( i ) ? ;");
                tb.add("let __makepad_json_result = Self (");
                add_tuple_de_values(&mut tb, types.len());
                tb.add(") ;");
                tb.add("s . block_close ( i ) ? ;");
                tb.add("std :: result :: Result :: Ok ( __makepad_json_result )");
            } else if let Some(fields) = parser.eat_all_struct_fields() {
                let fields = collect_json_fields(&fields);
                add_named_de_parse(&mut tb, &fields);
                tb.add("std :: result :: Result :: Ok ( Self {");
                add_named_de_values(&mut tb, &fields);
                tb.add("} )");
            } else {
                return parser.unexpected();
            }

            tb.add("} } ;");
            return tb.end();
        }
    } else if parser.eat_ident("enum") {
        if let Some(name) = parser.eat_any_ident() {
            let generic = parser.eat_generic();
            let where_clause = parser.eat_where_clause(Some("DeJson"));

            tb.add("impl").stream(generic.clone());
            tb.add("DeJson for")
                .ident(&name)
                .stream(generic)
                .stream(where_clause);
            tb.add("{ # [ inline ] fn de_json ( s : & mut DeJsonState , i : & mut std :: str :: Chars )");
            tb.add("-> std :: result :: Result < Self , DeJsonErr > {");
            tb.add("s . curly_open ( i ) ? ;");
            tb.add("s . string ( i ) ? ;");
            tb.add("s . colon ( i ) ? ;");
            tb.add("let __makepad_json_result = match s . strbuf . as_str ( ) {");

            if !parser.open_brace() {
                return parser.unexpected();
            }

            while !parser.eat_eot() {
                parser.eat_attributes();
                if let Some(variant) = parser.eat_any_ident() {
                    tb.string(&variant).add("=> {");

                    if let Some(types) = parser.eat_all_types() {
                        tb.add("s . block_open ( i ) ? ;");
                        tb.add("let __makepad_json_variant = Self ::")
                            .ident(&variant)
                            .add("(");
                        add_tuple_de_values(&mut tb, types.len());
                        tb.add(") ;");
                        tb.add("s . block_close ( i ) ? ;");
                        tb.add("__makepad_json_variant");
                    } else if let Some(fields) = parser.eat_all_struct_fields() {
                        let fields = collect_json_fields(&fields);
                        add_named_de_parse(&mut tb, &fields);
                        tb.add("Self ::").ident(&variant).add("{");
                        add_named_de_values(&mut tb, &fields);
                        tb.add("}");
                    } else if parser.is_punct_alone(',') || parser.is_eot() {
                        tb.add("s . block_open ( i ) ? ; s . block_close ( i ) ? ; Self ::")
                            .ident(&variant);
                    } else {
                        return parser.unexpected();
                    }

                    tb.add("}");
                    parser.eat_punct_alone(',');
                } else {
                    return parser.unexpected();
                }
            }

            tb.add("_ => return std :: result :: Result :: Err ( s . err_exp ( & s . strbuf ) )");
            tb.add("} ;");
            tb.add("s . curly_close ( i ) ? ;");
            tb.add("std :: result :: Result :: Ok ( __makepad_json_result )");
            tb.add("} } ;");
            return tb.end();
        }
    }

    parser.unexpected()
}
