//! Scraping batch multithread — lance plusieurs sites de doc en parallèle.
//!
//! Chaque site est crawlé COMPLÈTEMENT (toute la doc sous le préfixe de chemin),
//! avec son propre rate-limit poli interne. Le parallélisme se fait ENTRE sites
//! (N workers), pas à l'intérieur d'un site (on reste respectueux par domaine).
//!
//! Config : un fichier texte simple, une ligne par site :
//!     nom | url | max_pages
//!   - lignes vides et `#` ignorées
//!   - max_pages optionnel (défaut 500 = "toute la doc" pour la plupart des sites)
//!
//! Presets de workers : light=2, normal=4, turbo=8.

use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Une entrée de site à scraper.
#[derive(Clone)]
pub struct SiteJob {
    pub name: String,
    pub url: String,
    pub max_pages: usize,
    /// Langue à conserver : "en" (défaut, filtre les autres), "fr" (garde FR pour
    /// la conformité), "*" (toutes langues, aucun filtre). 4e colonne de la config.
    pub lang: String,
}

/// Résout un nom de preset en nombre de workers.
pub fn preset_workers(preset: &str) -> usize {
    match preset.to_ascii_lowercase().as_str() {
        "light" | "leger" => 2,
        "turbo" | "max" => 8,
        _ => 4, // normal
    }
}

/// Parse un fichier de config batch. Retourne les jobs valides + les erreurs de ligne.
pub fn parse_config(content: &str, default_max: usize) -> (Vec<SiteJob>, Vec<String>) {
    let mut jobs = Vec::new();
    let mut errors = Vec::new();
    for (lineno, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Format : nom | url | max? | lang?  (max et lang optionnels)
        let parts: Vec<&str> = line.split('|').map(|p| p.trim()).collect();
        if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
            errors.push(format!("ligne {} ignorée (format attendu: nom | url | max? | lang?): {}", lineno + 1, line));
            continue;
        }
        let max = parts.get(2).and_then(|m| m.parse::<usize>().ok()).unwrap_or(default_max);
        let lang = parts.get(3).map(|l| l.to_ascii_lowercase()).filter(|l| !l.is_empty()).unwrap_or_else(|| "en".to_string());
        jobs.push(SiteJob { name: parts[0].to_string(), url: parts[1].to_string(), max_pages: max, lang });
    }
    (jobs, errors)
}

/// Lance le scraping batch avec `workers` sites en parallèle.
/// Affiche la progression sur stderr (terminé/total) au fil de l'eau et retourne
/// les résultats (le caller affiche le récap final).
pub fn run_batch(jobs: &[SiteJob], workers: usize) -> Vec<(String, Result<usize, String>)> {
    let total = jobs.len();
    let done = Arc::new(AtomicUsize::new(0));
    let t0 = Instant::now();

    eprintln!("▶ Batch : {} sites · {} workers parallèles", total, workers);

    let pool = rayon::ThreadPoolBuilder::new().num_threads(workers.max(1)).build().expect("threadpool batch");

    let results = pool.install(|| {
        jobs.par_iter()
            .map(|job| {
                let st = Instant::now();
                // Progression live throttlée : 1 ligne / ~2s / site (lisible en parallèle).
                let mut last_tick = Instant::now();
                let mut first = true;
                let mut progress = |pages: usize, queue: usize| {
                    if first || last_tick.elapsed().as_millis() >= 2000 {
                        eprintln!("  ⟳ {:<16} {} pages · {} en file · {:.0}s", job.name, pages, queue, st.elapsed().as_secs_f64());
                        last_tick = Instant::now();
                        first = false;
                    }
                };
                let res = crate::scrape::scrape_site_progress(&job.url, &job.name, job.max_pages, &job.lang, &mut progress);
                let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                match &res {
                    Ok(pages) => eprintln!("  [{}/{}] ✓ {:<16} {} pages · {:.0}s", n, total, job.name, pages, st.elapsed().as_secs_f64()),
                    Err(e) => eprintln!("  [{}/{}] ✗ {:<16} ÉCHEC: {}", n, total, job.name, e),
                }
                (job.name.clone(), res)
            })
            .collect::<Vec<_>>()
    });

    let ok = results.iter().filter(|(_, r)| r.is_ok()).count();
    let pages: usize = results.iter().filter_map(|(_, r)| r.as_ref().ok()).sum();
    eprintln!("■ Batch terminé en {:.0}s · {}/{} sites OK · {} pages au total", t0.elapsed().as_secs_f64(), ok, total, pages);
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let cfg = "# commentaire\nReact | https://react.dev/reference | 50\n\nTauri | https://tauri.app/\nbad line\nCNIL | https://cnil.fr/ | 300 | fr\n";
        let (jobs, errors) = parse_config(cfg, 500);
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0].name, "React");
        assert_eq!(jobs[0].max_pages, 50);
        assert_eq!(jobs[0].lang, "en"); // défaut
        assert_eq!(jobs[1].max_pages, 500); // défaut
        assert_eq!(jobs[2].lang, "fr"); // 4e colonne
        assert_eq!(errors.len(), 1); // "bad line"
    }

    #[test]
    fn presets() {
        assert_eq!(preset_workers("light"), 2);
        assert_eq!(preset_workers("normal"), 4);
        assert_eq!(preset_workers("turbo"), 8);
        assert_eq!(preset_workers("inconnu"), 4);
    }
}
