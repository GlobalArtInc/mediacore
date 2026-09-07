/**
 * Whether a level that was measured is somebody talking.
 *
 * This is the whole of the speaking decision, kept out of the DOM and out of
 * Web Audio so `node --test` can argue with the numbers directly instead of
 * inferring them from a browser. `level-meter.js` measures; this decides; and
 * `app.js` owns neither judgement.
 *
 * It replaces a fixed threshold, and the reason is that a fixed threshold
 * cannot be right. The page used to call anything above an RMS of 0.02 speech,
 * which is -34 dBFS. Measured on the subscriber side of an undistorted
 * continuous tone: 0.0239 read "speaking", 0.0197 read "listening", 0.0100 read
 * "listening". A quiet or distant speaker lives permanently under that line
 * while everybody in the room can hear them perfectly well.
 *
 * Lowering the constant does not fix it, it inverts it. A room with a fan next
 * to a large condenser microphone sits at -40 dBFS with nobody in it, so a
 * threshold low enough for the quiet speaker paints every row "speaking" for
 * ever -- which is the worse lie of the two, because a roster that always says
 * yes cannot be caught being wrong. Every trade-off below is decided that way:
 * missing a speaker is a mistake somebody can see and that corrects itself,
 * and a permanent "speaking" is neither.
 *
 * So two facts are measured, not one.
 *
 *   - **How loud this room usually is**, tracked per meter. Speech is then a
 *     number of decibels above *that*, which fits a whispering laptop and a
 *     shouting broadcast microphone without either being configured.
 *   - **How long the sound has lasted.** A keystroke and a voice are the same
 *     height for fifty milliseconds and stop being the same thing well before
 *     half a second: the keystroke is over, the syllable is still going. Level
 *     alone cannot separate them at any threshold, which is why the first shape
 *     of this module painted a note-taker as a talker for the whole meeting --
 *     measured, keys thirty decibels over a quiet room read "speaking" 100 per
 *     cent of the time.
 *
 * Around those, three smaller rules:
 *
 *   - **Attack and release are different levels**, so a voice sitting exactly
 *     at the boundary does not strobe the roster between two answers, and half
 *     a second of hold carries the answer across the gap between two syllables.
 *   - **Nothing under an absolute level is speech**, however far above the room
 *     it sits, because a floor-relative test alone has no opinion about a track
 *     carrying nothing but dither.
 *   - **A reading nobody has refreshed is unknown**, not "listening". A meter
 *     that has stopped running knows nothing about the room, and the page says
 *     that rather than reporting the silence of its own machinery as the
 *     silence of a person.
 *
 * Everything here except staleness runs on the *audio* clock -- the audio the
 * meter actually measured, which is what the reports carry -- because that is
 * the time the sound happened in. Wall time only decides whether a reading is
 * too old to repeat, which is a question about this page and not about the
 * room.
 *
 * Nothing here imports anything, on purpose: the behaviour suite loads this
 * file straight off disk, where the page's absolute `/module.js` paths do not
 * resolve.
 */

/**
 * How far above the room a level has to be before it is called speech, and how
 * far it has to fall back before it stops being called speech. Nine decibels is
 * a little over three times the power of the room, which no fan and no hum
 * reaches and which ordinary speech clears by a wide margin -- the quiet
 * speaker measured at -40 dBFS in a -60 dBFS room is twenty decibels up. The
 * release is three decibels lower so a voice resting on the line settles on one
 * answer rather than flickering between two.
 */
export const ATTACK_OVER_FLOOR_DB = 9;
export const RELEASE_OVER_FLOOR_DB = 6;

/**
 * How long a sound has to go on before it can start speech.
 *
 * This is the other half of the question, and without it the first half is
 * unanswerable. A keystroke, a mouse click, a cup on a desk and a chair leg are
 * all far above any room a floor-relative test could learn -- a key measured
 * thirty decibels over a quiet room -- and no threshold separates them from the
 * first syllable of a word, because for the first fifty milliseconds they *are*
 * the first syllable of a word. What separates them is that fifty milliseconds
 * later the keystroke has stopped and the syllable has not.
 *
 * It is measured as *occupancy* rather than as an unbroken run, and that is not
 * a detail: syllables are 120 to 260 milliseconds and the dips between them are
 * real, so a rule that wanted a fifth of a second of unbroken loudness could
 * not be started by a short syllable at all -- measured, it cost ordinary
 * speech seventeen points of coverage. Two hundred milliseconds of loudness out
 * of any four hundred is a duty cycle of one half. Speech runs at about seventy
 * per cent; a keystroke every four hundred milliseconds runs at twelve, and a
 * fast typist at fifty-millisecond intervals of a hundred and fifty still only
 * reaches thirty-three.
 *
 * It is also what makes the noise floor safe to leave alone: the alternative to
 * a duration test is a floor that chases intermittent clatter upwards, and a
 * floor raised to the keyboard is one a quiet voice can no longer clear.
 *
 * Only the *start* is guarded this way. Once somebody is talking the hold and
 * the lower release level carry them across the gaps, because by then the
 * question has already been answered.
 */
export const ONSET_MS = 200;
export const ONSET_WINDOW_MS = 400;

/**
 * How long speech is held after the last block that cleared the release level.
 * A syllable boundary is a real gap in the signal, and a roster that goes dark
 * between two words is reporting the waveform rather than the person.
 */
export const HOLD_MS = 500;

/**
 * The one absolute rule left. A floor-relative test alone cannot tell a room
 * from a track carrying nothing but dither, and the first faint thing after a
 * silent stretch would otherwise be "far above the room" on the strength of
 * there having been no room. Nothing under this is speech however far above the
 * floor it sits. It is far below any voice worth showing -- the quiet speaker
 * that started all this measured -40 dBFS, twenty-five decibels above this
 * line.
 */
export const QUIETEST_SPEECH_DBFS = -65;

/**
 * The quietest a *room* can be, which is a different claim from the quietest a
 * voice can be, and the one that keeps a dropout from rewriting the floor.
 *
 * A microphone in a room, however still, is measuring air, a preamplifier and a
 * converter; sixteen bits of dither alone is about -96 dBFS and any real
 * capture path sits well above that. A level at or under -80 dBFS is therefore
 * not a quiet room, it is the track not carrying the room: a hardware-muted
 * headset whose samples are zeros, an Opus DTX gap, a jitter buffer that ran
 * dry, our own denoise gate writing zeros until the wasm is live. None of those
 * are evidence about how loud the room is, so none of them are allowed into the
 * estimate of it.
 *
 * Without this rule a single hardware mute lowers the floor for as long as it
 * lasts, and every real sound afterwards is thirty or forty decibels "above the
 * room" until the floor climbs back -- which is a quarter of a minute of a
 * roster saying somebody is talking in a room where nobody spoke.
 *
 * It is also what the floor reads before it has heard anything at all, so a
 * level arriving against no history at all is judged against a room that could
 * exist rather than against negative infinity.
 */
export const QUIETEST_ROOM_DBFS = -80;

/**
 * The floor's window, how many buckets it is kept in, and where among them the
 * floor sits.
 *
 * Four seconds is long enough that continuous speech still contains a gap and
 * short enough that a room which really does change is believed quickly. Each
 * half-second bucket holds the quietest level of its own half second, which is
 * what makes the estimate land on the room rather than on the voice: a syllable
 * is under three hundred milliseconds, so every bucket of somebody talking
 * contains a gap between two of them, and the bucket's minimum is the room.
 *
 * The floor is then the **median of the buckets**, and the whole robustness of
 * the estimate is in that word. The previous shape of this module took the
 * minimum of them, which is one number decided by one bucket: a single half
 * second in which the track carried nothing rewrote the room, and every real
 * sound for the next quarter of a minute was tens of decibels "above" it. A
 * median cannot be moved by anything that does not occupy more than half the
 * window, so a dropout has to last longer than two seconds before it moves the
 * estimate at all, and a room that has genuinely changed -- which occupies all
 * of the window soon enough -- still moves it.
 *
 * The two halves do different work and both are needed. Minima *within* a
 * bucket find the gaps in speech; a median *across* buckets refuses a
 * contiguous excursion. Either alone gets one of the two cases wrong.
 */
export const FLOOR_WINDOW_MS = 4000;
export const FLOOR_BUCKETS = 8;
export const FLOOR_PERCENTILE = 0.5;

/**
 * How fast the floor may move, in either direction. This is the number that
 * makes the floor an estimate of a room rather than a copy of the last four
 * seconds, and it is symmetric on purpose.
 *
 * Rooms do not change level quickly. A fan spins up, a window opens, somebody
 * moves the microphone: seconds, not milliseconds. So anything that appears to
 * change the room faster than a few decibels a second is the *track* changing,
 * not the room, and the estimate should not follow it.
 *
 * It is symmetric because the two directions are the same measurement. The
 * previous shape of this module rate-limited only the climb, on the reasoning
 * that a level below the floor is direct evidence the room is quieter -- and it
 * is, but evidence about the *track* is not evidence about the room, and one
 * fifty-millisecond window of digital silence therefore bought seventeen
 * seconds of "speaking" in a room where nobody spoke.
 *
 * Nine decibels a second, measured, is what that buys and costs:
 *
 *   - a room that gets *louder* and is briefly read as a person talking is
 *     corrected in the jump divided by nine, plus the two seconds the buckets
 *     need: a twenty-decibel jump in four seconds, a forty-decibel one in six;
 *   - a room that gets *quieter* is believed at the same speed, so somebody
 *     talking quietly after a loud speaker is heard about three seconds later
 *     rather than being lost under a floor left where the loud one put it;
 *   - and it is the whole of what protects a voice the far end has compressed
 *     flat. A speaker with two decibels of modulation and no breath for eight
 *     seconds reads "speaking" 99 per cent of the time with this limit and
 *     31 per cent without it -- deleting the limit is the one mutation that
 *     changes that number, which is why there is a test standing on it.
 */
export const FLOOR_MOVE_DB_PER_SECOND = 9;

/** What a window with no signal in it at all is called, so every level is a number. */
export const SILENCE_DBFS = -100;

/**
 * How old a reading may be before the answer built on it is withdrawn. The
 * meter reports every fifty milliseconds, so this is twenty missed reports: not
 * a slow frame, not a busy render, but a meter that has stopped.
 */
export const STALE_AFTER_MS = 1000;

export const NOTHING_MEASURED =
  "nothing has measured this audio yet, so whether they are speaking is not something this page " +
  "can say";

/**
 * Why an answer was withdrawn, in the words of the thing that went wrong. The
 * age is in the sentence because "the meter stopped four minutes ago" and "the
 * meter stopped a second ago" are different pieces of news to whoever is
 * hovering over the row.
 */
export function meterStopped(ageMs) {
  return (
    `nothing has measured this audio for ${formatAge(ageMs)}, so whether they are speaking is no ` +
    "longer something this page can claim"
  );
}

function formatAge(ageMs) {
  const seconds = ageMs / 1000;
  if (seconds < 10) return `${seconds.toFixed(1)} seconds`;
  if (seconds < 120) return `${Math.round(seconds)} seconds`;
  return `${Math.round(seconds / 60)} minutes`;
}

/**
 * One meter's opinion of one participant.
 *
 * `observe(report, at)` takes a report from `level-meter.js` -- an RMS over a
 * window of audio, and how many milliseconds of audio that window was -- and
 * the wall-clock reading of when it arrived here. It returns the same thing
 * `describe` would return immediately afterwards, because the caller almost
 * always wants it.
 *
 * `describe(now)` answers with the state as of `now`, which is where a reading
 * that has gone stale becomes `unknown`. Asking is cheap and does not move
 * anything, so the page asks on every repaint rather than caching an answer
 * that could quietly age.
 */
export function createVoiceGate(options = {}) {
  const attackDb = number(options.attackOverFloorDb, ATTACK_OVER_FLOOR_DB);
  const releaseDb = number(options.releaseOverFloorDb, RELEASE_OVER_FLOOR_DB);
  const onsetMs = number(options.onsetMs, ONSET_MS);
  const onset = createOnsetWindow(number(options.onsetWindowMs, ONSET_WINDOW_MS));
  const holdMs = number(options.holdMs, HOLD_MS);
  const quietestSpeech = number(options.quietestSpeechDbfs, QUIETEST_SPEECH_DBFS);
  const staleAfterMs = number(options.staleAfterMs, STALE_AFTER_MS);
  const floor = createFloorTracker(options);

  let audioMs = 0;
  let heldUntilAudioMs = 0;
  let talking = false;
  let level = null;
  let lastAt = null;

  function observe(report, at) {
    const windowMs = positive(report && report.windowMs) || 0;
    const measured = amplitude(report && report.rms);
    const arrivedAt = typeof at === "number" && Number.isFinite(at) ? at : null;

    // A gap in the reports longer than the staleness limit is a gap in which
    // this page had already withdrawn its answer -- a suspended context in a
    // background tab is the ordinary way to get one, and it can last minutes.
    // Whatever was being held was withdrawn out loud, so it must not come back
    // silently: a returning tab used to spend the rest of a hold on a syllable
    // that had ended a minute earlier. The floor is kept, because the room the
    // meter was in is still the room it is in.
    if (arrivedAt !== null && lastAt !== null && arrivedAt - lastAt > staleAfterMs) {
      heldUntilAudioMs = audioMs;
      onset.forget();
      talking = false;
    }

    audioMs += windowMs;
    lastAt = arrivedAt === null ? lastAt : arrivedAt;

    // Zero is a measurement of a *track* and not of a room, which is why it is
    // reported rather than dropped and why it is still kept out of the floor.
    // A window the browser handed no channels for is not even that, and both
    // are time in which nothing about the room arrived -- so a hold started
    // before one of them still runs out on schedule.
    level = measured === null ? SILENCE_DBFS : decibels(measured);
    floor.observe(level, windowMs);

    const over = level - floor.value;
    const speaks = level >= quietestSpeech;
    const sustained = onset.observe(speaks && over >= attackDb, windowMs) >= onsetMs;

    // Starting needs the attack level *and* the duration; carrying on needs
    // only the release level, because the question of whether this is a person
    // has already been answered and re-asking it every fifty milliseconds is
    // what makes a roster strobe.
    const clears = speaks && over >= (talking ? releaseDb : attackDb);
    if (clears && (talking || sustained)) heldUntilAudioMs = audioMs + holdMs;
    talking = audioMs < heldUntilAudioMs;

    return describe(lastAt);
  }

  function describe(now) {
    if (lastAt === null || level === null) {
      return {
        state: "unknown",
        speaking: false,
        level: null,
        floor: null,
        over: null,
        ageMs: null,
        reason: NOTHING_MEASURED,
        detail: null,
      };
    }

    const ageMs = typeof now === "number" && Number.isFinite(now) ? Math.max(0, now - lastAt) : 0;
    const over = level - floor.value;

    if (ageMs > staleAfterMs) {
      return {
        state: "unknown",
        speaking: false,
        level,
        floor: floor.value,
        over,
        ageMs,
        reason: meterStopped(ageMs),
        detail: null,
      };
    }

    return {
      state: talking ? "speaking" : "listening",
      speaking: talking,
      level,
      floor: floor.value,
      over,
      ageMs,
      reason: null,
      detail: describeLevel(level, floor.value),
    };
  }

  return { observe, describe };
}

/**
 * How much of the last few hundred milliseconds was loud, which is the question
 * `ONSET_MS` is asked against. A plain ring of the reports that fall in the
 * window: there are eight of them at the meter's cadence, and an approximation
 * would be harder to read than the thing itself.
 */
function createOnsetWindow(windowMs) {
  const loud = [];
  const spans = [];
  let held = 0;
  let loudMs = 0;

  return {
    observe(isLoud, elapsedMs) {
      loud.push(isLoud);
      spans.push(elapsedMs);
      held += elapsedMs;
      if (isLoud) loudMs += elapsedMs;
      while (loud.length > 1 && held - spans[0] >= windowMs) {
        held -= spans[0];
        if (loud[0]) loudMs -= spans[0];
        loud.shift();
        spans.shift();
      }
      return loudMs;
    },
    forget() {
      loud.length = 0;
      spans.length = 0;
      held = 0;
      loudMs = 0;
    },
  };
}

/**
 * The sentence a person gets on hover, and the one a bug report should be able
 * to quote. Both halves are needed: a level on its own says nothing about
 * whether it is speech, and a floor on its own says nothing about whether
 * anybody is talking.
 */
export function describeLevel(level, floorDbfs) {
  if (typeof level !== "number" || typeof floorDbfs !== "number") return null;
  const over = level - floorDbfs;
  const sign = over >= 0 ? "above" : "below";
  return (
    `${level.toFixed(1)} dBFS, ${Math.abs(over).toFixed(1)} dB ${sign} this room's own ` +
    `${floorDbfs.toFixed(1)} dBFS`
  );
}

/**
 * The room: the median of the last few seconds' worth of half-second minima,
 * followed at a bounded speed.
 *
 * Three separate things keep this honest, and each of them exists because
 * leaving it out produced a roster that lied.
 *
 * **Only levels a room could have produce it.** Anything at or under
 * `QUIETEST_ROOM_DBFS` is the track failing to carry the room rather than a
 * very quiet room, and it is dropped on the way in. A window of digital silence
 * therefore leaves the estimate exactly where it was, which is the only answer
 * that survives a hardware-muted headset.
 *
 * **A median of the buckets rather than their minimum.** A minimum is whatever
 * the single most extreme bucket claims; a median needs more than half the
 * window before it moves at all. This is what makes a dropout cost nothing and
 * a real change in the room cost only the time it takes to fill the window.
 *
 * **A bounded speed, the same in both directions.** Rooms change over seconds.
 * Anything faster is the track, and the estimate does not follow it. This is
 * also the number that decides how long a fan that starts mid-call is mistaken
 * for a person -- the jump divided by the rate -- so it is the one knob that
 * trades that mistake against how quickly a quieter room is believed.
 *
 * The first window is deliberately different. Until the buckets are full the
 * floor simply *is* the median of the buckets there are, because there is no
 * history to be careful with and the alternative is a few seconds of made-up
 * answers at the moment somebody joins: a meter attaching in a noisy room would
 * call the noise speech until the rate limit caught up, and one attaching
 * mid-sentence would take seconds to find the room instead of finding it at the
 * first gap between two syllables. Before anything at all has been admitted the
 * floor reads `QUIETEST_ROOM_DBFS`, so a level arriving against no history is
 * judged against a room that could exist rather than against nothing.
 */
export function createFloorTracker(options = {}) {
  const windowMs = number(options.floorWindowMs, FLOOR_WINDOW_MS);
  const buckets = Math.max(2, Math.round(number(options.floorBuckets, FLOOR_BUCKETS)));
  const percentile = number(options.floorPercentile, FLOOR_PERCENTILE);
  const moveDbPerSecond = number(options.floorMoveDbPerSecond, FLOOR_MOVE_DB_PER_SECOND);
  const quietestRoom = number(options.quietestRoomDbfs, QUIETEST_ROOM_DBFS);
  const bucketMs = windowMs / buckets;

  const closed = [];
  let filling = Number.POSITIVE_INFINITY;
  let filledMs = 0;
  let floor = null;

  function observe(level, elapsedMs) {
    if (level <= quietestRoom) return floor === null ? quietestRoom : floor;

    if (level < filling) filling = level;
    filledMs += elapsedMs;
    if (filledMs >= bucketMs) {
      closed.push(filling);
      while (closed.length > buckets - 1) closed.shift();
      filling = Number.POSITIVE_INFINITY;
      filledMs = 0;
    }

    const window = Number.isFinite(filling) ? [...closed, filling] : [...closed];
    const target = window.length === 0 ? level : rankedAt(window, percentile);

    if (floor === null || closed.length < buckets - 1) {
      floor = target;
    } else {
      const step = (moveDbPerSecond * elapsedMs) / 1000;
      const move = target - floor;
      floor += move > step ? step : move < -step ? -step : move;
    }
    if (floor < quietestRoom) floor = quietestRoom;
    return floor;
  }

  return {
    observe,
    get value() {
      return floor === null ? quietestRoom : floor;
    },
  };
}

/**
 * The `fraction` quantile of a handful of levels, by rank rather than by
 * interpolation: these are decibels and the answer should be one of the buckets
 * that was actually measured, not a number halfway between two of them. Sorting
 * a copy of eight values once per report is not worth an incremental structure.
 */
function rankedAt(window, fraction) {
  const sorted = [...window].sort((a, b) => a - b);
  const rank = Math.floor(fraction * sorted.length);
  return sorted[rank < sorted.length ? rank : sorted.length - 1];
}

/**
 * Amplitude to dBFS, with digital silence given a number rather than negative
 * infinity: every level in this module takes part in arithmetic, and one that
 * is not a number poisons the floor it lands in.
 */
export function decibels(amplitude) {
  if (typeof amplitude !== "number" || !Number.isFinite(amplitude) || amplitude <= 0) {
    return SILENCE_DBFS;
  }
  const value = 20 * Math.log10(amplitude);
  return value < SILENCE_DBFS ? SILENCE_DBFS : value;
}

function number(value, fallback) {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function positive(value) {
  if (typeof value !== "number" || !Number.isFinite(value) || value <= 0) return null;
  return value;
}

/** An amplitude the meter reported, zero included; null when it reported none. */
function amplitude(value) {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) return null;
  return value;
}
