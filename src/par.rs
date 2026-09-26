//! Parallélisme — UN seul réglage pour tout le code parallèle de Cortex.
//!
//! Tout ce qui est parallèle passe par le pool rayon GLOBAL (`par_iter`,
//! `rayon::join`) ou lit `threads()` (parcours `ignore`) : l'option
//! `--threads N` (ou la variable `CORTEX_THREADS`) configure ce pool au
//! démarrage, et le banc `bench-latence --echelle` exécute chaque mesure dans
//! un pool de N threads (`with_threads`) pour tracer la courbe 1/2/4/8/16.
//! Sans réglage : un thread par cœur logique (défaut de rayon).

/// Configure le pool global : `requested` (option `--threads`), sinon
/// `CORTEX_THREADS`, sinon le défaut de rayon. À appeler une fois, au démarrage.
pub fn init_global(requested: Option<usize>) {
    let n = requested.or_else(|| std::env::var("CORTEX_THREADS").ok().and_then(|v| v.trim().parse().ok())).filter(|&n| n > 0);
    if let Some(n) = n {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(n).build_global();
    }
}

/// Nombre de threads du pool courant (global, ou celui de `with_threads`).
pub fn threads() -> usize {
    rayon::current_num_threads()
}

/// Exécute `f` dans un pool de `n` threads : tout `par_iter` et tout appel à
/// `threads()` fait depuis `f` utilise ce pool.
pub fn with_threads<R: Send>(n: usize, f: impl FnOnce() -> R + Send) -> R {
    rayon::ThreadPoolBuilder::new().num_threads(n.max(1)).build().expect("pool rayon").install(f)
}
