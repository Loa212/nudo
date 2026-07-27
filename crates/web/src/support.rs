//! The "support the project" prompts.
//!
//! Ported from Coolify's sponsorship popup, keeping the two things it gets
//! right and dropping the two it does not.
//!
//! Kept: the dismissal is **monthly recurring rather than permanent** — the
//! button says "Maybe next time", not "Never", and it comes back the following
//! calendar month. And there is **one instance-wide off-switch** in settings
//! that stops it for good, so anyone who genuinely does not want it can say so
//! once.
//!
//! Dropped: Coolify's dismissal lives in `localStorage`, so it resets on every
//! new browser and cannot be honoured across devices — here it is stored against
//! the user. And Coolify's instance sends a "still alive" ping home on every
//! boot, on by default. nudo sends nothing, ever: there is no telemetry in this
//! codebase and no setting to disable, because there is nothing to disable.

use nudo_server::store::Store;

/// Where the prompts point.
pub struct SupportLinks;

impl SupportLinks {
    pub const SPONSOR: &'static str = "https://github.com/sponsors/Loa212";
    pub const REPOSITORY: &'static str = "https://github.com/Loa212/nudo";
    pub const ISSUES: &'static str = "https://github.com/Loa212/nudo/issues/new/choose";
    pub const DISCUSSIONS: &'static str = "https://github.com/Loa212/nudo/discussions";
}

/// Whether the banner should be shown to this user right now.
///
/// Three things have to hold: the instance has not turned it off, the user has
/// not dismissed it this calendar month, and — the point of the whole feature —
/// they have actually used nudo for something. Asking for support before someone
/// has deployed anything is asking a stranger for money.
pub async fn should_prompt(store: &Store, user_id: &str) -> bool {
    if !store.support_prompt_enabled().await.unwrap_or(true) {
        return false;
    }

    // Nothing deployed yet: they have not seen it work, so there is nothing to
    // support. This is a deliberate departure from Coolify, whose popup can
    // appear on a completely empty instance.
    let deployments = store.count_successful_deployments().await.unwrap_or(0);
    if deployments < MIN_DEPLOYMENTS_BEFORE_PROMPTING {
        return false;
    }

    match store.support_prompt_dismissed_at(user_id).await {
        Ok(Some(dismissed_at)) => is_a_new_month(dismissed_at, chrono::Utc::now()),
        // Never dismissed, or the lookup failed — showing it once too often is
        // a smaller error than never showing it at all.
        _ => true,
    }
}

/// How much someone must have done before being asked to support the project.
///
/// Low enough to be reached by anyone actually using it; high enough that a
/// first look at the dashboard is not interrupted by an ask.
const MIN_DEPLOYMENTS_BEFORE_PROMPTING: i64 = 5;

/// Whether a dismissal has aged into a new calendar month.
///
/// Calendar month rather than a rolling 30 days, following Coolify: it means the
/// prompt is predictable — at most once a month, near the start — instead of
/// drifting later each time.
pub fn is_a_new_month(
    dismissed_at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    use chrono::Datelike;
    (now.year(), now.month()) != (dismissed_at.year(), dismissed_at.month())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(year: i32, month: u32, day: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(year, month, day, 12, 0, 0)
            .single()
            .expect("a valid instant")
    }

    #[test]
    fn a_dismissal_lasts_the_rest_of_the_calendar_month() {
        // Dismissed on the 1st, still dismissed on the 28th.
        assert!(!is_a_new_month(at(2026, 3, 1), at(2026, 3, 28)));
        // Same day.
        assert!(!is_a_new_month(at(2026, 3, 15), at(2026, 3, 15)));
    }

    #[test]
    fn the_prompt_returns_the_following_month() {
        assert!(is_a_new_month(at(2026, 3, 31), at(2026, 4, 1)));
        assert!(is_a_new_month(at(2026, 1, 15), at(2026, 2, 1)));
    }

    #[test]
    fn a_year_boundary_counts_as_a_new_month() {
        // A naive month-only comparison would treat December and the following
        // December as the same month.
        assert!(is_a_new_month(at(2026, 12, 20), at(2027, 1, 5)));
        assert!(is_a_new_month(at(2026, 3, 1), at(2027, 3, 1)));
    }

    #[tokio::test]
    async fn nobody_is_asked_before_they_have_deployed_anything() {
        // Asking for support on an empty instance is asking a stranger for
        // money. Coolify's popup does exactly that; this one does not.
        let store = Store::open_in_memory().await.expect("store");
        assert!(!should_prompt(&store, "usr_1").await);
    }

    #[tokio::test]
    async fn the_instance_wide_switch_turns_it_off_for_good() {
        let store = Store::open_in_memory().await.expect("store");
        store
            .set_support_prompt_enabled(false)
            .await
            .expect("disable");
        assert!(!should_prompt(&store, "usr_1").await);
        assert!(!store.support_prompt_enabled().await.expect("read"));
    }

    #[tokio::test]
    async fn the_switch_defaults_to_on_and_can_be_turned_back_on() {
        let store = Store::open_in_memory().await.expect("store");
        assert!(store.support_prompt_enabled().await.expect("read"));

        store.set_support_prompt_enabled(false).await.expect("off");
        assert!(!store.support_prompt_enabled().await.expect("read"));

        store.set_support_prompt_enabled(true).await.expect("on");
        assert!(store.support_prompt_enabled().await.expect("read"));
    }

    #[tokio::test]
    async fn a_dismissal_is_recorded_per_user_not_per_browser() {
        // Coolify keeps this in localStorage, so it resets on every new browser
        // and cannot be honoured across devices.
        let store = Store::open_in_memory().await.expect("store");
        // A real user: the dismissal is keyed to one, so it is removed with them.
        let user = store
            .create_user("a@b.com", "correct horse battery", "A")
            .await
            .expect("user");

        store
            .dismiss_support_prompt(&user.id)
            .await
            .expect("dismiss");

        let dismissed = store
            .support_prompt_dismissed_at(&user.id)
            .await
            .expect("read");
        assert!(dismissed.is_some());

        // A different user is unaffected.
        assert!(
            store
                .support_prompt_dismissed_at("usr_2")
                .await
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn every_link_points_at_this_project() {
        for link in [
            SupportLinks::SPONSOR,
            SupportLinks::REPOSITORY,
            SupportLinks::ISSUES,
            SupportLinks::DISCUSSIONS,
        ] {
            assert!(link.starts_with("https://"), "{link} is not https");
            // Case-insensitive: GitHub resolves either spelling, so pinning one
            // would fail the next time the owner's casing is corrected rather
            // than catching a link that points somewhere else.
            assert!(
                link.to_lowercase().contains("loa212"),
                "{link} does not point at this project"
            );
        }
    }

    #[test]
    fn every_link_is_a_page_that_has_to_exist_on_the_repository() {
        // These are checked by hand against the live repository when they
        // change, because a unit test cannot fetch them — the module has no
        // network client, which is the point of the test below.
        //
        // What this pins is the *shape*, so a typo or a path that was never
        // enabled is visible here. Discussions shipped as a 404 for exactly
        // this reason: the link was written before the feature was turned on.
        assert_eq!(SupportLinks::SPONSOR, "https://github.com/sponsors/Loa212");
        assert_eq!(SupportLinks::REPOSITORY, "https://github.com/Loa212/nudo");
        assert_eq!(
            SupportLinks::ISSUES,
            "https://github.com/Loa212/nudo/issues/new/choose"
        );
        assert_eq!(
            SupportLinks::DISCUSSIONS,
            "https://github.com/Loa212/nudo/discussions"
        );
    }

    #[test]
    fn nothing_in_this_module_talks_to_the_network() {
        // nudo sends nothing home — no install count, no usage ping, nothing.
        // Coolify's equivalent phones home on every boot, on by default. This
        // test exists so the absence stays deliberate: adding a client here
        // means editing this assertion, which is a conversation rather than an
        // accident.
        //
        // Only the code above the tests is scanned, so this list cannot match
        // itself.
        let source = include_str!("support.rs");
        let code = source
            .split("#[cfg(test)]")
            .next()
            .expect("the module has a non-test half");

        for forbidden in [
            "reqwest",
            "HttpClient",
            "TcpStream",
            "phone_home",
            "posthog",
        ] {
            assert!(
                !code.contains(forbidden),
                "{forbidden} appears in this module — nudo does not phone home"
            );
        }
    }
}
