use super::*;

/// Reference `render_braille([1, 0, 1j], 4, 4)`: the first frame.
#[test]
fn the_snake_starts_as_the_reference_draws_it() {
    assert_eq!(LoadingAnimation::seeded(7).spinner(), "⠋ ");
}

#[test]
fn the_snake_stays_on_its_grid_and_never_crosses_itself() {
    for seed in 1..20 {
        let mut animation = LoadingAnimation::seeded(seed);
        for _ in 0..500 {
            animation.tick();
            assert!((SNAKE_LENGTH..=SNAKE_LENGTH + 1).contains(&animation.snake.len()));
            assert!(animation.snake.iter().all(|dot| in_grid(*dot)));
            let mut dots = animation.snake.clone();
            dots.sort_unstable();
            dots.dedup();
            assert_eq!(dots.len(), animation.snake.len());
            assert_eq!(animation.spinner().chars().count(), 2);
        }
    }
}

/// Reference `_update_animation`: the sweep crosses the spinner, the status
/// and its ellipsis, then the palette steps on and bounces at either end.
#[test]
fn the_color_sweep_bounces_across_the_palette() {
    let mut animation = LoadingAnimation::seeded(3);
    animation.status("Reading", 1, 1);
    let mut indexes = vec![animation.color_index];
    for _ in 0..9 {
        // Nine cells ("Reading" plus the spinner and the ellipsis), then the
        // step past them.
        for _ in 0..=9 {
            animation.tick();
        }
        indexes.push(animation.color_index);
    }
    assert_eq!(indexes, [0, 1, 2, 3, 4, 3, 2, 1, 0, 1]);
    assert_eq!(animation.color_at(0), TARGET_COLORS[1]);
    animation.tick();
    assert_eq!(animation.color_at(0), TARGET_COLORS[2]);
    assert_eq!(animation.color_at(5), TARGET_COLORS[1]);
}

/// Reference `_with_easter_egg`: only the generic statuses are replaced, and
/// re-setting a status never rerolls it.
#[test]
fn only_generic_statuses_are_ever_replaced_and_never_rerolled() {
    let mut replaced = 0;
    for seed in 1..400 {
        let mut animation = LoadingAnimation::seeded(seed);
        assert_eq!(animation.status("Reading file", 12, 25), "Reading file");
        let shown = animation.status(DEFAULT_ACTIVITY_STATUS, 12, 25).to_owned();
        if shown != DEFAULT_ACTIVITY_STATUS {
            replaced += 1;
            assert!(
                EASTER_EGGS.contains(&shown.as_str()) || DECEMBER_EGGS.contains(&shown.as_str())
            );
        }
        assert_eq!(animation.status(DEFAULT_ACTIVITY_STATUS, 12, 25), shown);
    }
    // One time in ten, give or take.
    assert!((10..80).contains(&replaced), "{replaced}");
}

#[test]
fn the_calendar_day_is_read_off_the_unix_time() {
    assert_eq!(month_day(0), (1, 1));
    // 2025-10-31T12:00:00Z and 2024-02-29T00:00:00Z.
    assert_eq!(month_day(1_761_912_000_000), (10, 31));
    assert_eq!(month_day(1_709_164_800_000), (2, 29));
}

/// Reference `PetitChat`: a step every 160 ms, a rest of five to twenty
/// seconds at the end of the cycle, and a freeze that waits for the cycle to
/// come back to its first frame.
#[test]
fn the_banner_cat_cycles_rests_and_freezes_on_its_first_frame() {
    let mut cat = PetitChat::new(true);
    assert_eq!(cat.frame(), CAT_FRAMES[0]);
    cat.advance(0);
    cat.advance(159);
    assert_eq!(cat.step, 0);
    cat.advance(160);
    assert_eq!(cat.frame(), CAT_FRAMES[1]);
    // A whole cycle, then the rest it ends with.
    let mut now = 160;
    while cat.step != 0 {
        now = cat.next_ms.expect("scheduled");
        cat.advance(now);
    }
    let resumes = cat.next_ms.expect("scheduled") - now;
    assert!((5_160..=20_160).contains(&resumes), "{resumes}");

    cat.freeze();
    cat.advance(now + 30_000);
    assert!(cat.stopped);
    assert_eq!(cat.frame(), CAT_FRAMES[0]);

    let mut still = PetitChat::new(false);
    still.advance(60_000);
    assert_eq!(still.step, 0);
}
