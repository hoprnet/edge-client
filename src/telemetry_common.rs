use tracing_subscriber::prelude::*;

pub(crate) fn build_base_subscriber() -> anyhow::Result<
    impl tracing::Subscriber + Send + Sync + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
> {
    // When `RUST_LOG` is unset we fall back to a sane default: `info` globally,
    // which keeps operational logs visible without drowning them in library
    // noise. The per-target `=info` overrides below pin chatty transport/DB
    // crates to `info` too, so that raising the global level to `debug`/`trace`
    // via `RUST_LOG` does not flood the output with their internals unless the
    // operator explicitly asks for them.
    let env_filter = match tracing_subscriber::EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => tracing_subscriber::filter::EnvFilter::new("info")
            .add_directive("libp2p_swarm=info".parse()?)
            .add_directive("libp2p_mplex=info".parse()?)
            .add_directive("libp2p_tcp=info".parse()?)
            .add_directive("libp2p_dns=info".parse()?)
            .add_directive("multistream_select=info".parse()?)
            .add_directive("isahc=error".parse()?)
            .add_directive("sea_orm=warn".parse()?)
            .add_directive("sqlx=warn".parse()?)
            .add_directive("hyper_util=warn".parse()?),
    };

    #[cfg(feature = "prof")]
    let env_filter = env_filter
        .add_directive("tokio=trace".parse()?)
        .add_directive("runtime=trace".parse()?);

    let use_json = std::env::var("HOPRD_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    let format: Box<dyn tracing_subscriber::Layer<_> + Send + Sync> = {
        let base = tracing_subscriber::fmt::layer()
            .with_level(true)
            .with_target(true)
            .with_thread_ids(true)
            .with_thread_names(false);
        if use_json {
            base.json().boxed()
        } else {
            base.boxed()
        }
    };

    #[cfg(feature = "prof")]
    let prof_layer = console_subscriber::spawn();
    #[cfg(not(feature = "prof"))]
    let prof_layer = tracing_subscriber::layer::Identity::new();

    Ok(tracing_subscriber::Registry::default()
        .with(env_filter)
        .with(prof_layer)
        .with(format))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned")
    }

    fn set_env_var(key: &str, value: &str) {
        unsafe {
            std::env::set_var(key, value);
        }
    }

    fn remove_env_var(key: &str) {
        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn build_base_subscriber_ok_without_format_env() {
        let _guard = env_lock();
        remove_env_var("HOPRD_LOG_FORMAT");
        assert!(build_base_subscriber().is_ok());
    }

    #[test]
    fn build_base_subscriber_ok_with_json_format() {
        let _guard = env_lock();
        set_env_var("HOPRD_LOG_FORMAT", "json");
        let result = build_base_subscriber();
        remove_env_var("HOPRD_LOG_FORMAT");
        assert!(result.is_ok());
    }

    #[test]
    fn build_base_subscriber_ok_with_plain_format() {
        let _guard = env_lock();
        set_env_var("HOPRD_LOG_FORMAT", "plain");
        let result = build_base_subscriber();
        remove_env_var("HOPRD_LOG_FORMAT");
        assert!(result.is_ok());
    }
}
