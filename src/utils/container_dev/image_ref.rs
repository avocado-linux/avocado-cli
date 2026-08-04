//! One place that decides what an image reference means.
//!
//! Three copies of this logic used to live in `watcher.rs` (`repo_and_tag`,
//! `with_default_tag`) and `ws.rs` (`split_image_tag`), and they drifted: the
//! cross-arch guard recorded an image's architecture under the RAW event ref and
//! looked it up under the registry-stripped one, so `arch_for` returned `None`
//! for every registry-qualified ref and the broadcast filter fell through to its
//! permissive arm. podman qualifies local refs as `localhost/my-app:dev`, so on
//! podman that was every ref - an amd64 image reached an aarch64 device, which is
//! the case the guard exists to prevent.
//!
//! Two normal forms, deliberately distinct, because they answer different
//! questions:
//!
//! - [`canonical`] strips the registry and applies the default tag. It is the
//!   identity of an image as a THING, and the right key for anything that has to
//!   agree across the watcher and the control WS.
//! - [`with_default_tag`] alone leaves the registry in place. It is the identity
//!   of a ref as CONFIGURED, and `WatchSet` must keep using it: the push retags
//!   to `<registry>/<repo>:<tag>` on the way to every push, so a watch set keyed
//!   on the canonical form would match the watcher's own side effect and drive a
//!   retag -> event -> sync -> retag loop.

/// Strip a leading registry component (`localhost/…`, `host.tld/…`,
/// `host:port/…`), leaving `repo[:tag]`.
///
/// A first path segment is a registry only if it looks like a host: podman's
/// `localhost`, or something carrying a dot or a port colon. `library/alpine` has
/// neither, so it stays whole.
pub fn strip_registry(image: &str) -> &str {
    match image.split_once('/') {
        Some((first, rest))
            if first == "localhost" || first.contains('.') || first.contains(':') =>
        {
            rest
        }
        _ => image,
    }
}

/// `repo` -> `repo:latest`, leaving an already-tagged ref alone.
///
/// Only a colon AFTER the last `/` is a tag separator; a colon before it belongs
/// to a registry `host:port` (`host:5601/repo`).
pub fn with_default_tag(image: &str) -> String {
    let name_start = image.rfind('/').map_or(0, |i| i + 1);
    if image[name_start..].contains(':') {
        image.to_string()
    } else {
        format!("{image}:latest")
    }
}

/// The normal form used as a cross-module key: registry stripped, tag defaulted.
///
/// `localhost/my-app:dev`, `my-app:dev` and `10.0.2.2:5000/my-app:dev` all
/// canonicalize to `my-app:dev`, so a value recorded on one path is found on
/// another regardless of which engine produced the ref.
pub fn canonical(image: &str) -> String {
    with_default_tag(strip_registry(image))
}

/// Split an image reference into `(repo, tag)` in [`canonical`] form.
pub fn split(image: &str) -> (String, String) {
    let canonical = canonical(image);
    match canonical.rsplit_once(':') {
        Some((repo, tag)) => (repo.to_string(), tag.to_string()),
        // `canonical` always appends a tag, so this is unreachable in practice.
        None => (canonical, "latest".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_agrees_across_engine_ref_shapes() {
        // The bug this module exists for: the arch book recorded under the raw
        // ref and read back under the stripped one, so the two never matched for
        // a podman user. Every shape an engine can report has to land on one key.
        for raw in [
            "my-app:dev",
            "localhost/my-app:dev",
            "10.0.2.2:5000/my-app:dev",
            "registry.example.com/my-app:dev",
        ] {
            assert_eq!(canonical(raw), "my-app:dev", "canonical({raw:?})");
        }
    }

    #[test]
    fn canonical_applies_the_default_tag() {
        assert_eq!(canonical("my-app"), "my-app:latest");
        assert_eq!(canonical("localhost/my-app"), "my-app:latest");
    }

    #[test]
    fn a_registry_port_colon_is_not_a_tag_separator() {
        assert_eq!(with_default_tag("host:5601/repo"), "host:5601/repo:latest");
    }

    #[test]
    fn a_bare_namespace_is_not_a_registry() {
        // `library` has no dot, no colon, and is not `localhost`, so stripping it
        // would silently rewrite the image the user asked for.
        assert_eq!(strip_registry("library/alpine"), "library/alpine");
        assert_eq!(canonical("library/alpine"), "library/alpine:latest");
    }

    #[test]
    fn with_default_tag_keeps_the_registry_that_watchset_needs() {
        // WatchSet keys on this form, NOT on `canonical`. The push retags to
        // `<registry>/<repo>:<tag>`, so canonicalizing here would make the
        // watcher match its own retag and re-enter its sync path forever.
        assert_eq!(
            with_default_tag("10.0.2.2:5000/my-app:dev"),
            "10.0.2.2:5000/my-app:dev"
        );
        assert_ne!(
            with_default_tag("10.0.2.2:5000/my-app:dev"),
            canonical("10.0.2.2:5000/my-app:dev")
        );
    }

    #[test]
    fn split_returns_canonical_components() {
        assert_eq!(
            split("localhost/my-app:dev"),
            ("my-app".to_string(), "dev".to_string())
        );
        assert_eq!(
            split("my-app"),
            ("my-app".to_string(), "latest".to_string())
        );
    }
}
