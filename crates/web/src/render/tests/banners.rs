use super::*;

pub(super) fn an_update(available: bool, breaking: bool) -> UpdateBanner {
    UpdateBanner {
        current: "0.1.0".to_string(),
        latest: "0.2.0".to_string(),
        available,
        breaking,
        url: "https://github.com/loa212/nudo/releases/tag/v0.2.0".to_string(),
    }
}

#[test]
fn a_current_instance_is_shown_no_update_banner_at_all() {
    // Not a hidden element or an empty box — nothing, so the dashboard of
    // someone who is up to date looks exactly as it did before.
    assert_eq!(s(update_banner(&an_update(false, false))), "");
}

#[test]
fn the_update_banner_names_both_versions_and_links_to_the_notes() {
    let rendered = s(update_banner(&an_update(true, false)));
    assert!(rendered.contains("0.2.0"), "the new version is not named");
    assert!(
        rendered.contains("0.1.0"),
        "the running version is not named"
    );
    assert!(rendered.contains("/changelog"));
    assert!(rendered.contains("releases/tag/v0.2.0"));
}

#[test]
fn the_update_banner_never_offers_to_perform_the_upgrade() {
    // Coolify's equivalent runs a downloaded script as root. nudo's banner
    // only ever links: `/upgrade` is a page of instructions, and the test
    // below asserts that page has no form and pipes nothing into a shell.
    // What is forbidden here is anything that would *act* — a POST, or a
    // command embedded in the banner itself.
    let rendered = s(update_banner(&an_update(true, true)));
    assert!(
        !rendered.contains("<form"),
        "the banner submits something, so it does more than link"
    );
    for forbidden in ["curl", "install.sh", "| sh"] {
        assert!(
            !rendered.contains(forbidden),
            "the banner contains {forbidden}, which would run code on the host"
        );
    }
}

#[test]
fn a_breaking_release_says_so_before_anyone_upgrades() {
    let rendered = s(update_banner(&an_update(true, true)));
    assert!(rendered.contains("manual steps"));
    assert!(
        rendered.contains("callout bad"),
        "it is not styled as a warning"
    );
}

#[test]
fn release_notes_from_the_manifest_cannot_inject_markup() {
    // The manifest is fetched over the network, so its notes are untrusted.
    let entry = ChangelogEntry {
        version: "9.9.9".to_string(),
        notes: "- <script>alert(1)</script>\n- <img src=x onerror=alert(1)>".to_string(),
        ..ChangelogEntry::default()
    };
    let rendered = s(changelog_page(&[entry], "0.1.0"));
    // Both are escaped, so they render as visible text rather than as
    // elements. `onerror=` still appears in the output — as the characters
    // of an escaped `<img ...>`, inside a `<li>`, where it is inert.
    assert!(!rendered.contains("<script>"), "a script tag survived");
    assert!(!rendered.contains("<img"), "an img tag survived");
    assert!(rendered.contains("&lt;script&gt;"));
    assert!(rendered.contains("&lt;img"));
}

#[test]
fn release_notes_render_bullets_as_a_list_and_prose_as_paragraphs() {
    let entry = ChangelogEntry {
        version: "0.2.0".to_string(),
        notes: "## Fixed\n\n- One thing\n- Another thing\n\nA closing note.".to_string(),
        ..ChangelogEntry::default()
    };
    let rendered = s(changelog_page(&[entry], "0.1.0"));
    assert!(rendered.contains("<li>One thing</li>"));
    assert!(rendered.contains("<li>Another thing</li>"));
    assert!(rendered.contains("<p>A closing note.</p>"));
    // The heading keeps its text but loses its hashes.
    assert!(rendered.contains("Fixed"));
    assert!(!rendered.contains("## Fixed"));
}

#[test]
fn the_changelog_marks_the_version_this_instance_is_running() {
    let entries = [
        ChangelogEntry {
            version: "0.2.0".to_string(),
            ..ChangelogEntry::default()
        },
        ChangelogEntry {
            version: "0.1.0".to_string(),
            current: true,
            ..ChangelogEntry::default()
        },
    ];
    let rendered = s(changelog_page(&entries, "0.1.0"));
    assert!(rendered.contains("running"));
}

#[test]
fn an_instance_that_has_never_checked_gets_an_explanation_not_an_error() {
    let rendered = s(changelog_page(&[], "0.1.0"));
    assert!(rendered.contains("No release notes yet"));
    // Nothing is wrong with the instance, and the page should say so.
    assert!(rendered.contains("Nothing is wrong"));
}

fn support_test_links() -> SupportLinkView<'static> {
    SupportLinkView {
        sponsor: "https://github.com/sponsors/loa212",
        repository: "https://github.com/loa212/nudo",
        issues: "https://github.com/loa212/nudo/issues/new/choose",
        discussions: "https://github.com/loa212/nudo/discussions",
    }
}

#[test]
fn the_support_banner_offers_a_way_out_that_is_not_a_purchase() {
    // Someone who cannot or will not sponsor should still find something
    // useful to do, and a dismissal that is one click away.
    let rendered = s(support_banner("tok", support_test_links()));
    assert!(rendered.contains("Sponsor"));
    assert!(rendered.contains("Star on GitHub"));
    assert!(rendered.contains("Report a bug"));
    assert!(rendered.contains("Maybe next time"));
    assert!(rendered.contains("/support/dismiss"));
}

#[test]
fn dismissing_the_support_banner_is_a_csrf_protected_post() {
    // A GET would let any page on the internet dismiss it for the user.
    let rendered = s(support_banner("tok_abc", support_test_links()));
    assert!(rendered.contains(r#"method="post""#));
    assert!(rendered.contains(r#"value="tok_abc""#));
}

#[test]
fn the_settings_page_carries_both_switches_and_says_nothing_is_sent() {
    let prefs = SettingsPrefs {
        update_check_enabled: true,
        support_prompt_enabled: false,
        last_checked: "2 hours ago".to_string(),
    };
    let rendered = s(settings_page(&[], "a@b.c", &prefs, "t"));
    assert!(rendered.contains("/settings/updates"));
    assert!(rendered.contains("/settings/support"));
    assert!(rendered.contains("2 hours ago"));
    // The claim that matters most on that page.
    assert!(rendered.contains("no usage"));
}

#[test]
fn an_unticked_switch_renders_unticked() {
    // Rendering a stored `false` as a ticked box would silently turn the
    // setting back on the next time anyone saved the form.
    let off = SettingsPrefs::default();
    let rendered = s(settings_page(&[], "a@b.c", &off, "t"));
    let updates_form = rendered
        .split(r#"action="/settings/updates""#)
        .nth(1)
        .expect("the updates form is on the page");
    let form_body = updates_form.split("</form>").next().expect("a closed form");
    assert!(
        !form_body.contains("checked"),
        "a disabled release check renders as ticked"
    );
}
