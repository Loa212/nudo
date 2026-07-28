use super::*;

mod detail;
mod form;
mod list;

pub use detail::{service_detail, service_unit};
pub use form::service_form;
pub use list::{services_list, services_rows};

/// Escapes a value for interpolation into a single-quoted JavaScript string
/// inside an attribute.
///
/// Maud escapes the attribute for HTML, which neutralises `"`, `<` and `&`, but
/// not the `'` that would close the JS literal. Both layers are needed.
pub(super) fn js_text(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn javascript_text_is_escaped_for_both_layers() {
        assert_eq!(js_text("it's"), "it\\'s");
        assert_eq!(js_text("back\\slash"), "back\\\\slash");
        // Double quotes are left to maud's attribute escaping.
        assert_eq!(js_text("say \"hi\""), "say \"hi\"");
    }
}
