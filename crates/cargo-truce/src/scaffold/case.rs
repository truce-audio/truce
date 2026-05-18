//! `to_pascal_case` helper, used to derive struct names from crate names.

#[must_use]
pub fn to_pascal_case(s: &str) -> String {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_word() {
        assert_eq!(to_pascal_case("gain"), "Gain");
    }

    #[test]
    fn hyphenated() {
        assert_eq!(to_pascal_case("demo-effect"), "DemoEffect");
    }

    #[test]
    fn snake_case_is_camelcased() {
        // Crate names with underscores (`demo_effect`) must split on the
        // separator - `Demo_effect` would trip rustc's
        // `non_camel_case_types` warning on every generated struct.
        assert_eq!(to_pascal_case("demo_effect"), "DemoEffect");
    }

    #[test]
    fn mixed_separators() {
        assert_eq!(to_pascal_case("foo_bar-baz"), "FooBarBaz");
    }

    #[test]
    fn empty_segments_dropped() {
        assert_eq!(to_pascal_case("foo--bar"), "FooBar");
        assert_eq!(to_pascal_case("__foo"), "Foo");
    }
}
