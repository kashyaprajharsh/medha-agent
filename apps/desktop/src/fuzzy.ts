const boundary = (text: string, at: number) =>
  at === 0 || /[\s/:._-]/.test(text[at - 1]);

/** How well one word fits a name: lower is closer, `null` is no fit. */
function fit(text: string, word: string) {
  const whole = text.indexOf(word);
  if (whole >= 0) {
    let at = whole;
    while (at >= 0 && !boundary(text, at)) at = text.indexOf(word, at + 1);
    return at >= 0 ? at / 1000 : 10 + whole / 1000;
  }
  // Letters in order with small gaps: "gmni" still finds "gemini". The
  // tightest run wins, and a run spread across the whole name is not a fit.
  let tightest = Infinity;
  for (
    let first = text.indexOf(word[0]);
    first >= 0;
    first = text.indexOf(word[0], first + 1)
  ) {
    let last = first;
    for (const letter of word.slice(1)) {
      last = text.indexOf(letter, last + 1);
      if (last < 0) break;
    }
    if (last < 0) break;
    tightest = Math.min(tightest, last - first + 1);
  }
  return tightest <= word.length * 2 + 1 ? 100 + tightest : null;
}

/** Items whose name fits every typed word, closest first; an empty query keeps them all in order. */
export function ranked<T>(items: T[], query: string, name: (item: T) => string) {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return items;
  const scored: { item: T; score: number; at: number }[] = [];
  items.forEach((item, at) => {
    const text = name(item).toLowerCase();
    let score = 0;
    for (const word of words) {
      const next = fit(text, word);
      if (next === null) return;
      score += next;
    }
    scored.push({ item, score, at });
  });
  return scored
    .sort((a, b) => a.score - b.score || a.at - b.at)
    .map((entry) => entry.item);
}
