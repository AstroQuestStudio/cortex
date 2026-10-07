//! Dashboard TUI du scraping batch — UI fixe dans le terminal (façon app).
//!
//! Tableau réécrit EN PLACE (curseur ANSI), 1 ligne/site, rafraîchi ~6×/s en ne
//! repeignant QUE ce qui change. Contrôles clavier LIVE, ciblés sur le site
//! sélectionné (▸) :
//!   ↑/↓   sélection du site
//!   ←/→   concurrence du site sélectionné (vitesse intra-site)
//!   +/-   budget (max pages) du site sélectionné
//!   p     pause du site sélectionné        r  reprise du site sélectionné
//!   R     RELANCE un site terminé/coupé (avec son budget courant)
//!   [/]   délai global entre requêtes (ms)
//!   q     quitter proprement (sauvegarde + récap)
//!
//! Le pool de workers globaux borne le nombre de sites crawlés simultanément.

use crate::batch::SiteJob;
use crate::engine::{crawl_site, Controls, SiteState};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    terminal, QueueableCommand,
};
use std::io::{Stdout, Write};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn run_dashboard(jobs: &[SiteJob], workers: usize, concurrency: usize, delay_ms: u64, auto_refill: bool) {
    let started = Instant::now();
    let controls = Controls::new(delay_ms, auto_refill);
    let states: Vec<Arc<SiteState>> =
        jobs.iter().map(|j| SiteState::new(&j.name, &j.url, j.max_pages, concurrency, &j.lang, started)).collect();

    // File de travail partagée : index du prochain site à démarrer.
    let work: Arc<Mutex<VecLikeQueue>> = Arc::new(Mutex::new(VecLikeQueue::new(states.len())));
    let mut handles = Vec::new();
    for _ in 0..workers.max(1) {
        let work = Arc::clone(&work);
        let controls = Arc::clone(&controls);
        let states = states.clone();
        handles.push(std::thread::spawn(move || loop {
            let idx = {
                let mut q = work.lock().unwrap();
                q.pop()
            };
            match idx {
                Some(i) if !controls.quit.load(Ordering::Relaxed) => {
                    crawl_site(Arc::clone(&states[i]), Arc::clone(&controls));
                }
                _ => break,
            }
        }));
    }

    // Boucle UI + clavier.
    let mut stdout = std::io::stdout();
    let _ = terminal::enable_raw_mode();
    let _ = stdout.queue(cursor::Hide);
    let _ = stdout.flush();

    let mut selected = 0usize;
    let mut prev_lines: Vec<String> = Vec::new();
    let mut header_drawn = false;

    let mut quitting = false;
    loop {
        // Poll court (50ms) → clavier réactif. On draine TOUS les events en attente
        // ce tour-ci (sinon une touche spammée s'accumule et réagit en retard).
        while event::poll(Duration::from_millis(0)).unwrap_or(false) {
            if let Ok(Event::Key(k)) = event::read() {
                // Windows émet Press ET Release → ne traiter que Press (sinon ×2).
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                let s = &states[selected];
                match k.code {
                    KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        if selected + 1 < states.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::Right => s.bump_concurrency(1),
                    KeyCode::Left => s.bump_concurrency(-1),
                    KeyCode::Char('+') | KeyCode::Char('=') => s.add_budget(100),
                    KeyCode::Char('-') | KeyCode::Char('_') => s.add_budget(-100),
                    KeyCode::Char(']') => controls.bump_delay(-25),
                    KeyCode::Char('[') => controls.bump_delay(25),
                    KeyCode::Char('a') => controls.toggle_auto_refill(),
                    KeyCode::Char('p') => s.paused.store(true, Ordering::Relaxed),
                    KeyCode::Char('r') => s.paused.store(false, Ordering::Relaxed),
                    KeyCode::Char('R') => {
                        if s.done.load(Ordering::Relaxed) {
                            s.done.store(false, Ordering::Relaxed);
                            s.truncated.store(false, Ordering::Relaxed);
                            s.paused.store(false, Ordering::Relaxed);
                            s.add_budget(500);
                            let st = Arc::clone(s);
                            let ct = Arc::clone(&controls);
                            handles.push(std::thread::spawn(move || crawl_site(st, ct)));
                        } else {
                            s.add_budget(500);
                        }
                    }
                    KeyCode::Char('q') | KeyCode::Esc => {
                        // Arrêt IMMÉDIAT côté UI : on signale quit et on sort de la boucle
                        // d'affichage tout de suite. Les workers s'arrêteront à leur
                        // prochain tour (≤ 1 fetch) ; on les join hors écran.
                        controls.quit.store(true, Ordering::Relaxed);
                        quitting = true;
                    }
                    _ => {}
                }
            }
        }

        if quitting {
            // Message immédiat, puis on sort : pas d'attente bloquante à l'écran.
            let _ = stdout.queue(cursor::MoveTo(0, (states.len() + 7) as u16));
            let _ = write!(stdout, "Stopping… (saving the pages already fetched)");
            let _ = stdout.flush();
            break;
        }

        let lines = render_lines(&states, &controls, selected, started, workers);
        draw_diff(&mut stdout, &lines, &mut prev_lines, &mut header_drawn);

        if states.iter().all(|s| s.done.load(Ordering::Relaxed)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    for h in handles {
        let _ = h.join();
    }
    let _ = stdout.queue(cursor::Show);
    let _ = stdout.flush();
    let _ = terminal::disable_raw_mode();
    print_recap(&states, started);
}

/// Petite file d'indices à traiter (FIFO), thread-safe via le Mutex englobant.
struct VecLikeQueue {
    items: std::collections::VecDeque<usize>,
}
impl VecLikeQueue {
    fn new(n: usize) -> Self {
        VecLikeQueue { items: (0..n).collect() }
    }
    fn pop(&mut self) -> Option<usize> {
        self.items.pop_front()
    }
}

fn render_lines(states: &[Arc<SiteState>], controls: &Controls, selected: usize, started: Instant, workers: usize) -> Vec<String> {
    let delay = controls.delay_ms.load(Ordering::Relaxed);
    let elapsed = started.elapsed().as_secs();
    let total_pages: usize = states.iter().map(|s| s.pages.load(Ordering::Relaxed)).sum();
    let done_count = states.iter().filter(|s| s.done.load(Ordering::Relaxed)).count();
    let active = states.iter().filter(|s| !s.done.load(Ordering::Relaxed) && s.pages_per_sec() > 0.0).count();

    let auto = if controls.auto_refill.load(Ordering::Relaxed) { "AUTO✓" } else { "auto✗" };
    let mut lines = Vec::new();
    lines.push(format!(
        "Cortex Batch · {} sites · {} in parallel (active {}) · delay {}ms · resume {} · {:02}:{:02}",
        states.len(),
        workers,
        active,
        delay,
        auto,
        elapsed / 60,
        elapsed % 60
    ));
    lines.push("─".repeat(78));
    lines.push(format!("{:<16} {:>6} {:>7} {:>4} {:>6} {:>6}  {:<16}", "Site", "Pages", "File", "Cc", "Max", "p/s", "Statut"));
    lines.push("─".repeat(78));

    for (i, s) in states.iter().enumerate() {
        let sel = if i == selected { "▸" } else { " " };
        let pages = s.pages.load(Ordering::Relaxed);
        let queued = s.queued.load(Ordering::Relaxed);
        let cc = s.concurrency.load(Ordering::Relaxed);
        let maxp = s.max_pages.load(Ordering::Relaxed);
        let pps = s.pages_per_sec();
        let status = if let Some(e) = s.error.lock().unwrap().as_ref() {
            format!("✗ {}", trunc(e, 14))
        } else if s.done.load(Ordering::Relaxed) {
            if s.truncated.load(Ordering::Relaxed) {
                "⚠ cut (R=resume)".to_string()
            } else {
                "✓ done".to_string()
            }
        } else if s.paused.load(Ordering::Relaxed) {
            "⏸ pause (r=reprise)".to_string()
        } else {
            bar(pages, maxp)
        };
        lines.push(format!(
            "{}{:<15} {:>6} {:>7} {:>4} {:>6} {:>6.1}  {:<16}",
            sel,
            trunc(&s.name, 15),
            pages,
            queued,
            cc,
            maxp,
            pps,
            status
        ));
    }

    lines.push("─".repeat(78));
    lines.push(format!("Total: {} pages · {}/{} sites done", total_pages, done_count, states.len()));
    lines.push("↑↓ select · ←→ speed · +/- budget · p/r pause · R resume · a AUTO-resume · [ ] delay · q quit".to_string());
    lines
}

fn bar(cur: usize, max: usize) -> String {
    let frac = if max > 0 { (cur as f64 / max as f64).min(1.0) } else { 0.0 };
    let filled = (frac * 8.0).round() as usize;
    format!("{}{} {:>3}%", "█".repeat(filled), "░".repeat(8 - filled), (frac * 100.0) as usize)
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn draw_diff(stdout: &mut Stdout, lines: &[String], prev: &mut Vec<String>, header_drawn: &mut bool) {
    if !*header_drawn {
        let _ = stdout.queue(cursor::MoveTo(0, 0));
        let _ = stdout.queue(terminal::Clear(terminal::ClearType::All));
        for line in lines {
            let _ = writeln!(stdout, "{}", line);
        }
        let _ = stdout.flush();
        *prev = lines.to_vec();
        *header_drawn = true;
        return;
    }
    for (i, line) in lines.iter().enumerate() {
        let changed = prev.get(i).map(|p| p != line).unwrap_or(true);
        if changed {
            let _ = stdout.queue(cursor::MoveTo(0, i as u16));
            let _ = stdout.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            let _ = write!(stdout, "{}", line);
        }
    }
    let _ = stdout.flush();
    *prev = lines.to_vec();
}

fn print_recap(states: &[Arc<SiteState>], started: Instant) {
    let total: usize = states.iter().map(|s| s.pages.load(Ordering::Relaxed)).sum();
    let ok = states.iter().filter(|s| s.done.load(Ordering::Relaxed) && s.error.lock().unwrap().is_none()).count();
    println!("\n── Batch recap ── ({:.0}s)", started.elapsed().as_secs_f64());
    for s in states {
        let pages = s.pages.load(Ordering::Relaxed);
        if let Some(e) = s.error.lock().unwrap().as_ref() {
            println!("  ✗ {:<16} {}", s.name, e);
        } else if s.truncated.load(Ordering::Relaxed) {
            println!("  ⚠ {:<16} {} pages ({} not fetched)", s.name, pages, s.queued.load(Ordering::Relaxed));
        } else {
            println!("  ✓ {:<16} {} pages", s.name, pages);
        }
    }
    println!("Total : {} pages · {}/{} sites OK", total, ok, states.len());
}
