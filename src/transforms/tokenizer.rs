use super::Transform;
use crate::{
    event::{self, Event},
    topology::config::{DataType, TransformConfig},
    types::{parse_check_conversion_map, Conversion},
};
use nom::{
    branch::alt,
    bytes::complete::{escaped, is_not, tag},
    character::complete::{one_of, space0},
    combinator::{all_consuming, map, opt, rest, verify},
    multi::many0,
    sequence::{delimited, terminated},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str;
use string_cache::DefaultAtom as Atom;

#[derive(Deserialize, Serialize, Debug, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TokenizerConfig {
    pub field_names: Vec<Atom>,
    pub field: Option<Atom>,
    pub drop_field: bool,
    pub types: HashMap<Atom, String>,
}

#[typetag::serde(name = "tokenizer")]
impl TransformConfig for TokenizerConfig {
    fn build(&self) -> Result<Box<dyn Transform>, String> {
        let field = self.field.as_ref().unwrap_or(&event::MESSAGE);

        let types = parse_check_conversion_map(&self.types, &self.field_names)
            .map_err(|err| format!("{}", err))?;

        // don't drop the source field if it's getting overwritten by a parsed value
        let drop_field = self.drop_field && !self.field_names.iter().any(|f| f == field);

        Ok(Box::new(Tokenizer::new(
            self.field_names.clone(),
            field.clone(),
            drop_field,
            types,
        )))
    }

    fn input_type(&self) -> DataType {
        DataType::Log
    }

    fn output_type(&self) -> DataType {
        DataType::Log
    }
}

pub struct Tokenizer {
    field_names: Vec<(Atom, Conversion)>,
    field: Atom,
    drop_field: bool,
}

impl Tokenizer {
    pub fn new(
        field_names: Vec<Atom>,
        field: Atom,
        drop_field: bool,
        types: HashMap<Atom, Conversion>,
    ) -> Self {
        let field_names = field_names
            .into_iter()
            .map(|name| {
                let conversion = types.get(&name).unwrap_or(&Conversion::Bytes).clone();
                (name, conversion)
            })
            .collect();

        Self {
            field_names,
            field,
            drop_field,
        }
    }
}

impl Transform for Tokenizer {
    fn transform(&mut self, mut event: Event) -> Option<Event> {
        let value = event.as_log().get(&self.field).map(|s| s.to_string_lossy());

        if let Some(value) = &value {
            for ((name, conversion), value) in self.field_names.iter().zip(parse(value).into_iter())
            {
                match conversion.convert(value.as_bytes().into()) {
                    Ok(value) => event.as_mut_log().insert_explicit(name.clone(), value),
                    Err(error) => {
                        debug!(
                            message = "Could not convert types.",
                            name = &name[..],
                            %error
                        );
                    }
                }
            }
            if self.drop_field {
                event.as_mut_log().remove(&self.field);
            }
        } else {
            debug!(
                message = "Field does not exist.",
                field = self.field.as_ref(),
            );
        };

        Some(event)
    }
}

pub fn parse(input: &str) -> Vec<&str> {
    let simple = is_not::<_, _, (&str, nom::error::ErrorKind)>(" \t[\"");
    let string = delimited(
        tag("\""),
        map(opt(escaped(is_not("\"\\"), '\\', one_of("\"\\"))), |o| {
            o.unwrap_or("")
        }),
        tag("\""),
    );
    let bracket = delimited(
        tag("["),
        map(opt(escaped(is_not("]\\"), '\\', one_of("]\\"))), |o| {
            o.unwrap_or("")
        }),
        tag("]"),
    );

    // fall back to returning the rest of the input, if any
    let remainder = verify(rest, |s: &str| s.len() > 0);
    let field = alt((bracket, string, simple, remainder));

    all_consuming(many0(terminated(field, space0)))(input)
        .expect("parser should always succeed")
        .1
}

#[cfg(test)]
mod tests {
    use super::parse;
    use super::TokenizerConfig;
    use crate::event::{LogEvent, ValueKind};
    use crate::{topology::config::TransformConfig, Event};
    use string_cache::DefaultAtom as Atom;

    #[test]
    fn basic() {
        assert_eq!(parse("foo"), &["foo"]);
    }

    #[test]
    fn multiple() {
        assert_eq!(parse("foo bar"), &["foo", "bar"]);
    }

    #[test]
    fn more_space() {
        assert_eq!(parse("foo\t bar"), &["foo", "bar"]);
    }

    #[test]
    fn quotes() {
        assert_eq!(parse(r#"foo "bar baz""#), &["foo", r#"bar baz"#]);
    }

    #[test]
    fn empty_quotes() {
        assert_eq!(parse(r#"foo """#), &["foo", ""]);
    }

    #[test]
    fn escaped_quotes() {
        assert_eq!(
            parse(r#"foo "bar \" \" baz""#),
            &["foo", r#"bar \" \" baz"#],
        );
    }

    #[test]
    fn unclosed_quotes() {
        assert_eq!(parse(r#"foo "bar"#), &["foo", "\"bar"],);
    }

    #[test]
    fn brackets() {
        assert_eq!(parse("[foo.bar = baz] quux"), &["foo.bar = baz", "quux"],);
    }

    #[test]
    fn empty_brackets() {
        assert_eq!(parse("[] quux"), &["", "quux"],);
    }

    #[test]
    fn escaped_brackets() {
        assert_eq!(
            parse(r#"[foo " [[ \] "" bar] baz"#),
            &[r#"foo " [[ \] "" bar"#, "baz"],
        );
    }

    #[test]
    fn unclosed_brackets() {
        assert_eq!(parse("foo [bar"), &["foo", "[bar"],);
    }

    #[test]
    fn truncated_field() {
        assert_eq!(
            parse("foo bar[baz]: quux"),
            &["foo", "bar", "baz", ":", "quux"]
        );
        assert_eq!(parse("foo bar[baz quux"), &["foo", "bar", "[baz quux"]);
    }

    #[test]
    fn dash_field() {
        assert_eq!(parse("foo - bar"), &["foo", "-", "bar"]);
    }

    #[test]
    fn from_fuzzing() {
        assert_eq!(parse("").len(), 0);
        assert_eq!(parse("f] bar"), &["f]", "bar"]);
        assert_eq!(parse("f\" bar"), &["f", "\" bar"]);
        assert_eq!(parse("f[f bar"), &["f", "[f bar"]);
        assert_eq!(parse("f\"f bar"), &["f", "\"f bar"]);
        assert_eq!(parse("[][x"), &["", "[x"]);
        assert_eq!(parse("x[][x"), &["x", "", "[x"]);
    }

    fn parse_log(
        text: &str,
        fields: &str,
        field: Option<&str>,
        drop_field: bool,
        types: &[(&str, &str)],
    ) -> LogEvent {
        let event = Event::from(text);
        let field_names = fields.split(' ').map(|s| s.into()).collect::<Vec<Atom>>();
        let field = field.map(|f| f.into());
        let mut parser = TokenizerConfig {
            field_names,
            field,
            drop_field,
            types: types.iter().map(|&(k, v)| (k.into(), v.into())).collect(),
            ..Default::default()
        }
        .build()
        .unwrap();

        parser.transform(event).unwrap().into_log()
    }

    #[test]
    fn tokenizer_adds_parsed_field_to_event() {
        let log = parse_log("1234 5678", "status time", None, false, &[]);

        assert_eq!(log[&"status".into()], "1234".into());
        assert_eq!(log[&"time".into()], "5678".into());
        assert!(log.get(&"message".into()).is_some());
    }

    #[test]
    fn tokenizer_does_drop_parsed_field() {
        let log = parse_log("1234 5678", "status time", Some("message"), true, &[]);

        assert_eq!(log[&"status".into()], "1234".into());
        assert_eq!(log[&"time".into()], "5678".into());
        assert!(log.get(&"message".into()).is_none());
    }

    #[test]
    fn tokenizer_does_not_drop_same_name_parsed_field() {
        let log = parse_log("1234 yes", "status message", Some("message"), true, &[]);

        assert_eq!(log[&"status".into()], "1234".into());
        assert_eq!(log[&"message".into()], "yes".into());
    }

    #[test]
    fn tokenizer_coerces_fields_to_types() {
        let log = parse_log(
            "1234 yes 42.3 word",
            "code flag number rest",
            None,
            false,
            &[("flag", "bool"), ("code", "integer"), ("number", "float")],
        );

        assert_eq!(log[&"number".into()], ValueKind::Float(42.3));
        assert_eq!(log[&"flag".into()], ValueKind::Boolean(true));
        assert_eq!(log[&"code".into()], ValueKind::Integer(1234));
        assert_eq!(log[&"rest".into()], ValueKind::Bytes("word".into()));
    }

    #[test]
    fn tokenizer_keeps_dash_as_dash() {
        let log = parse_log(
            "1234 - foo",
            "code who why",
            None,
            false,
            &[("code", "integer"), ("who", "string"), ("why", "string")],
        );
        assert_eq!(log[&"code".into()], ValueKind::Integer(1234));
        assert_eq!(log[&"who".into()], ValueKind::Bytes("-".into()));
        assert_eq!(log[&"why".into()], ValueKind::Bytes("foo".into()));
    }
}
