//! The terminal's two idle-time animations. Reference `LoadingWidget`
//! (`vibe/cli/textual_ui/widgets/loading.py`): the snake spinner, the color
//! sweep across the status, and the occasional playful status that replaces a
//! generic one. Reference `PetitChat`
//! (`vibe/cli/textual_ui/widgets/banner/petit_chat.py`): the banner's cat.

use super::diagnostics::DEFAULT_ACTIVITY_STATUS;

/// Reference `THINKING_LOADING_STATUS`.
pub const THINKING_ACTIVITY_STATUS: &str = "Thinking";

/// Reference `LoadingWidget.TARGET_COLORS`, as RGB: yellow, light orange,
/// orange, dark orange, red.
pub const TARGET_COLORS: [(u8, u8, u8); 5] = [
    (0xFF, 0xD8, 0x00),
    (0xFF, 0xAF, 0x00),
    (0xFF, 0x82, 0x05),
    (0xFA, 0x50, 0x0F),
    (0xE1, 0x05, 0x00),
];

/// Reference `EASTER_EGGS`, `EASTER_EGGS_HALLOWEEN` and `EASTER_EGGS_DECEMBER`.
const EASTER_EGGS: [&str; 13] = [
    "Eating a chocolatine",
    "Eating a pain au chocolat",
    "Réflexion",
    "Analyse",
    "Contemplation",
    "Synthèse",
    "Reading Proust",
    "Oui oui baguette",
    "Counting Rs in strawberry",
    "Seeding Mistral weights",
    "Vibing",
    "Sending good vibes",
    "Petting le chat",
];
const HALLOWEEN_EGGS: [&str; 6] = [
    "Trick or treating",
    "Carving pumpkins",
    "Summoning spirits",
    "Brewing potions",
    "Haunting the terminal",
    "Petting le chat noir",
];
const DECEMBER_EGGS: [&str; 5] = [
    "Wrapping presents",
    "Decorating the tree",
    "Drinking hot chocolate",
    "Building snowmen",
    "Writing holiday cards",
];

/// Reference `SnakeSpinner`: a three-dot snake wandering a four-by-four dot
/// grid, drawn as two braille cells.
const SNAKE_GRID: i32 = 4;
const SNAKE_LENGTH: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadingAnimation {
    /// Dots from head to tail, as `(x, y)`.
    snake: Vec<(i32, i32)>,
    color_index: usize,
    color_forward: bool,
    transition_progress: usize,
    /// The status as the turn named it, and as it is shown.
    base_status: String,
    shown_status: String,
    random: u64,
}

impl Default for LoadingAnimation {
    fn default() -> Self {
        Self::seeded(seed())
    }
}

impl LoadingAnimation {
    #[must_use]
    pub fn seeded(random: u64) -> Self {
        Self {
            snake: vec![(1, 0), (0, 0), (0, 1)],
            color_index: 0,
            color_forward: true,
            transition_progress: 0,
            base_status: String::new(),
            shown_status: String::new(),
            random: random | 1,
        }
    }

    /// The status to show for `status`. Reference `_set_status`: the same
    /// status never rerolls, and only the two generic ones may be replaced,
    /// one time in ten.
    pub fn status(&mut self, status: &str, month: u32, day: u32) -> &str {
        if status != self.base_status {
            self.base_status = status.to_owned();
            self.shown_status =
                if matches!(status, DEFAULT_ACTIVITY_STATUS | THINKING_ACTIVITY_STATUS)
                    && self.next_random().is_multiple_of(10)
                {
                    self.easter_egg(month, day).to_owned()
                } else {
                    status.to_owned()
                };
        }
        &self.shown_status
    }

    fn easter_egg(&mut self, month: u32, day: u32) -> &'static str {
        let mut eggs = EASTER_EGGS.to_vec();
        if month == 10 && day == 31 {
            eggs.extend(HALLOWEEN_EGGS);
        }
        if month == 12 {
            eggs.extend(DECEMBER_EGGS);
        }
        let index = usize::try_from(self.next_random() % eggs.len() as u64).unwrap_or(0);
        eggs[index]
    }

    /// Reference `LoadingWidget._update_animation`, run every tenth of a
    /// second: the snake moves, and the color sweep advances one cell across
    /// the spinner, the status and its ellipsis.
    pub fn tick(&mut self) {
        self.advance_snake();
        let total = self.shown_status.chars().count() + 2;
        self.transition_progress += 1;
        if self.transition_progress > total {
            self.color_index = self.next_color_index();
            if !(1..TARGET_COLORS.len() - 1).contains(&self.color_index) {
                self.color_forward = !self.color_forward;
            }
            self.transition_progress = 0;
        }
    }

    fn next_color_index(&self) -> usize {
        if self.color_forward {
            self.color_index + 1
        } else {
            self.color_index.saturating_sub(1)
        }
    }

    /// Reference `_get_color_for_position`: the cells the sweep has reached
    /// take the next color.
    #[must_use]
    pub fn color_at(&self, position: usize) -> (u8, u8, u8) {
        if position < self.transition_progress {
            TARGET_COLORS[self.next_color_index()]
        } else {
            TARGET_COLORS[self.color_index]
        }
    }

    /// Reference `render_braille` over the snake's dots.
    #[must_use]
    pub fn spinner(&self) -> String {
        let mut cells = [0u32; 2];
        for &(x, y) in &self.snake {
            let (sub_x, sub_y) = (x % 2, y % 4);
            let dot = if sub_y < 3 {
                sub_y + 1 + 3 * sub_x
            } else {
                7 + sub_x
            };
            cells[usize::try_from(x / 2).unwrap_or(0).min(1)] |= 1 << (dot - 1);
        }
        cells
            .iter()
            .map(|&dots| {
                if dots == 0 {
                    ' '
                } else {
                    char::from_u32(0x2800 + dots).unwrap_or(' ')
                }
            })
            .collect()
    }

    /// Reference `SnakeSpinner._next_positions`.
    fn advance_snake(&mut self) {
        if self.snake.len() > SNAKE_LENGTH {
            self.snake.truncate(SNAKE_LENGTH);
            return;
        }
        let head = self.snake[0];
        let current = (head.0 - self.snake[1].0, head.1 - self.snake[1].1);
        let direction = self.direction(current);
        // A turn grows the snake by a dot for one frame; going straight moves
        // it whole.
        self.snake
            .insert(0, (head.0 + direction.0, head.1 + direction.1));
        if direction == current {
            self.snake.pop();
        }
    }

    /// Reference `SnakeSpinner._get_direction`: a bent snake keeps going while
    /// it can; otherwise it goes straight or turns, at random, onto a free dot.
    fn direction(&mut self, current: (i32, i32)) -> (i32, i32) {
        let bent = {
            let xs = self.snake.iter().map(|dot| dot.0);
            let ys = self.snake.iter().map(|dot| dot.1);
            distinct(xs) > 1 && distinct(ys) > 1
        };
        let head = self.snake[0];
        let ahead = (head.0 + current.0, head.1 + current.1);
        if bent && in_grid(ahead) {
            return current;
        }
        // Multiplying by 1, i and -i: straight, then either turn.
        let candidates = [current, (-current.1, current.0), (current.1, -current.0)]
            .into_iter()
            .filter(|offset| {
                let dot = (head.0 + offset.0, head.1 + offset.1);
                in_grid(dot) && !self.snake.contains(&dot)
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return current;
        }
        let index = usize::try_from(self.next_random() % candidates.len() as u64).unwrap_or(0);
        candidates[index]
    }

    fn next_random(&mut self) -> u64 {
        xorshift(&mut self.random)
    }
}

/// Xorshift: the reference draws from `random`, which no observer can replay,
/// so any uniform source is as faithful.
fn xorshift(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    value
}

fn distinct(values: impl Iterator<Item = i32>) -> usize {
    let mut seen = values.collect::<Vec<_>>();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

const fn in_grid((x, y): (i32, i32)) -> bool {
    0 <= x && x < SNAKE_GRID && 0 <= y && y < SNAKE_GRID
}

/// The banner cat's distinct frames, as reference `render_braille` draws
/// them, and the frame each step of its cycle shows. Captured from the
/// reference renderer rather than recomputed from its dot tables.
const CAT_FRAMES: [[&str; 3]; 9] = [
    ["  ⡠⣒⠄  ⡔⢄⠔⡄", " ⢸⠸⣀⡔⢉⠱⣃⡢⣂⡣", "  ⠉⠒⠣⠤⠵⠤⠬⠮⠆"],
    ["  ⡠⣒⠄  ⡔⢄⠔⡄", " ⢸⠸⣀⡔⢉⠱⣃⡠⣀⡣", "  ⠉⠒⠣⠤⠵⠤⠬⠮⠆"],
    [" ⢠⢢    ⡔⢄⠔⡄", " ⢸⢸⣀⡔⢉⠱⣃⡢⣂⡣", " ⠈⠒⠒⠣⠤⠵⠤⠬⠮⠆"],
    [" ⢠⢢    ⡔⢄⠔⡄", " ⢸⢸⣀⡔⢉⠱⣃⡐⣔⡣", " ⠈⠒⠒⠣⠤⠵⠤⠬⠮⠆"],
    ["⢔⡢⡀    ⡔⢄⠔⡄", " ⢸⠸⣀⡔⢉⠱⣃⡐⣔⡣", "  ⠉⠒⠣⠤⠵⠤⠬⠮⠆"],
    [" ⢠⢢    ⡠⡀⡠⡀", " ⢸⢸⣀⡔⢉⢑⠇⠨⡠⢇", " ⠈⠒⠒⠣⠤⠵⠭⠭⠯⠇"],
    ["  ⡠⣒⠄  ⡠⡀⡠⡀", " ⢸⠸⣀⡔⢉⢑⠇⠨⡠⢇", "  ⠉⠒⠣⠤⠵⠭⠭⠯⠇"],
    ["  ⡠⣒⠄  ⡠⡀⡠⡀", " ⢸⠸⣀⡔⢉⢑⠇⠈⡀⢇", "  ⠉⠒⠣⠤⠵⠭⠭⠯⠇"],
    ["⢔⡢⡀    ⡠⡀⡠⡀", " ⢸⠸⣀⡔⢉⢑⠇⠨⡠⢇", "  ⠉⠒⠣⠤⠵⠭⠭⠯⠇"],
];
const CAT_CYCLE: [usize; 26] = [
    0, 1, 0, 0, 2, 3, 3, 4, 4, 3, 3, 5, 5, 6, 7, 6, 6, 5, 5, 8, 8, 4, 4, 3, 2, 2,
];
/// Reference `FRAME_INTERVAL_S`, `CYCLE_DELAY_MIN_S` and `CYCLE_DELAY_MAX_S`.
const CAT_STEP_MS: u64 = 160;
const CAT_PAUSE_MS: std::ops::RangeInclusive<u64> = 5_000..=20_000;
/// Reference `EYES_OPEN_PAUSE_FRAMES`: where the cat may rest mid-cycle, one
/// time in four.
const CAT_RESTING_STEPS: [usize; 4] = [5, 11, 21, 24];

/// Reference `PetitChat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PetitChat {
    step: usize,
    next_ms: Option<u64>,
    /// After a rest, a whole cycle plays before the cat may rest again.
    resume_step: Option<usize>,
    resting: bool,
    freeze_requested: bool,
    stopped: bool,
    random: u64,
}

impl Default for PetitChat {
    fn default() -> Self {
        Self::new(true)
    }
}

impl PetitChat {
    /// A cat that animates unless `disable_welcome_banner_animation` said not.
    #[must_use]
    pub fn new(animate: bool) -> Self {
        Self {
            step: 0,
            next_ms: None,
            resume_step: None,
            resting: false,
            freeze_requested: false,
            stopped: !animate,
            random: seed() | 1,
        }
    }

    #[must_use]
    pub fn frame(&self) -> [&'static str; 3] {
        CAT_FRAMES[CAT_CYCLE[self.step]]
    }

    /// Keeps the cat on the frame it shows.
    pub fn stop(&mut self) {
        self.stopped = true;
    }

    /// Reference `freeze_animation`, which the first submitted prompt calls:
    /// the cat finishes its cycle, or its rest, and stays still.
    pub fn freeze(&mut self) {
        self.freeze_requested = true;
    }

    /// Plays every step due by `now_ms`.
    pub fn advance(&mut self, now_ms: u64) {
        while !self.stopped {
            let next = *self.next_ms.get_or_insert(now_ms + CAT_STEP_MS);
            if now_ms < next {
                return;
            }
            self.play_step(next);
        }
    }

    /// Reference `_apply_next_transition` and `_pause_between_cycles`.
    fn play_step(&mut self, at_ms: u64) {
        if self.freeze_requested && (self.step == 0 || self.resting) {
            self.stopped = true;
            return;
        }
        self.resting = false;
        self.step = (self.step + 1) % CAT_CYCLE.len();
        self.next_ms = Some(at_ms + CAT_STEP_MS);
        if let Some(resume) = self.resume_step {
            if self.step != resume {
                return;
            }
            self.resume_step = None;
        }
        let rests = self.step == 0
            || (CAT_RESTING_STEPS.contains(&self.step) && self.next_random().is_multiple_of(4));
        if rests {
            let span = CAT_PAUSE_MS.end() - CAT_PAUSE_MS.start() + 1;
            let pause = CAT_PAUSE_MS.start() + self.next_random() % span;
            self.resume_step = Some(self.step);
            self.resting = true;
            self.next_ms = Some(at_ms + pause + CAT_STEP_MS);
        }
    }

    fn next_random(&mut self) -> u64 {
        xorshift(&mut self.random)
    }
}

/// The calendar month and day of a Unix time in milliseconds. The reference
/// reads the local calendar; the day is taken in UTC here, which moves an
/// October 31 or December label by at most a few hours.
#[must_use]
pub fn month_day(milliseconds: u64) -> (u32, u32) {
    // Howard Hinnant's `civil_from_days`.
    let days = i64::try_from(milliseconds / 86_400_000).unwrap_or(0) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// A unit test reads the generic status back, so it runs on a seed whose
/// draws never replace one.
#[cfg(test)]
const fn seed() -> u64 {
    1
}

#[cfg(not(test))]
fn seed() -> u64 {
    let nanos = vibe_core::clock::now_nanos();
    u64::try_from(nanos % u128::from(u64::MAX)).unwrap_or(1)
}

#[cfg(test)]
mod loading_tests;
