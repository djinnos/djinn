//! Framework classifier — maps well-known dependency names to canonical
//! framework slugs. Keep the mapping flat + explicit; adding a new
//! framework is a one-line edit.

/// Look up a dependency name (e.g. `"react"`) and return the canonical
/// framework slug that dep indicates, or `None` if it's not one we
/// recognise.
pub fn framework_for_dep(name: &str) -> Option<&'static str> {
    match name {
        // JS / TS
        "react" | "react-dom" => Some("react"),
        "next" => Some("next"),
        "vue" => Some("vue"),
        "svelte" | "@sveltejs/kit" => Some("svelte"),
        "@angular/core" => Some("angular"),
        "solid-js" => Some("solid"),
        "astro" => Some("astro"),
        "remix" | "@remix-run/node" => Some("remix"),
        "nuxt" => Some("nuxt"),
        "express" => Some("express"),
        "fastify" => Some("fastify"),
        "hono" => Some("hono"),
        "nestjs" | "@nestjs/core" => Some("nestjs"),

        // Rust
        "axum" => Some("axum"),
        "actix-web" => Some("actix"),
        "rocket" => Some("rocket"),
        "warp" => Some("warp"),
        "tokio" => Some("tokio"),
        "leptos" => Some("leptos"),
        "dioxus" => Some("dioxus"),
        "tauri" => Some("tauri"),

        // Python
        "fastapi" => Some("fastapi"),
        "flask" => Some("flask"),
        "django" | "Django" => Some("django"),
        "starlette" => Some("starlette"),
        "sanic" => Some("sanic"),

        // Ruby
        "rails" => Some("rails"),
        "sinatra" => Some("sinatra"),

        _ => None,
    }
}

/// Collapse + sort a bag of framework slugs, deduplicating.
pub fn canonicalize(mut slugs: Vec<&'static str>) -> Vec<String> {
    slugs.sort();
    slugs.dedup();
    slugs.into_iter().map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_deps() {
        assert_eq!(framework_for_dep("react"), Some("react"));
        assert_eq!(framework_for_dep("axum"), Some("axum"));
        assert!(framework_for_dep("lodash").is_none());
    }
}
