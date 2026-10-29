# Images, screenshots and PDFs

A delegated run **sees pictures**, not bytes. There is no argument for it: put the file where the
run can reach it and name it in the prompt.

## How to hand one over

Either works, and both are verified:

- put it in the **`workspace`**, or
- pass its folder via **`add_dirs`** and write the absolute path in the prompt.

Same as any large input. A single image costs roughly **1–5k ctx**.

## What can be read

| format | result |
|---|---|
| PNG / JPG / WEBP | **seen as a picture** |
| PDF with a text layer | **read** — text verbatim, table structure, page count and page size |
| SVG | **not rendered.** It arrives as source text; the run says so rather than pretending |
| corrupt / unsupported image | **the run dies with `exit 1`**, it does not invent an answer |

For that last row the bridge checks the images your prompt names when a run dies that way, and the
error says `image that may not decode: <path>`. An image found some other way is not checked, so an
unexplained `exit 1` right after an image still points at the image, not agy.

## What to trust

Reliable, and reproduced across sizes:

- **Verbatim text**, down to very small type — 11px Japanese, and the same UI scaled to about 6px,
  both transcribed exactly including a nonsense identifier that could not be guessed.
- **Code screenshots, character for character** — line numbers, identifiers, `format!` braces,
  backticked comments. It also volunteers structural observations (no syntax highlighting, a line
  that is cut off mid-function).
- **Control states** — checked/unchecked, what each input field contains.
- **Spatial structure** — which box is where, which way each arrow points. A deliberately planted
  return path that *skipped a component* was identified correctly.
- **Values off a chart, and arithmetic on them.**
- **Image dimensions**, exactly.
- **Large flat fills** — a background or a body-text colour comes back exact.

## What not to trust

- **Pixel coordinates.** A button's real box came back with the vertical nearly right and the
  horizontal off by about 6% of the image width. Good enough for "roughly there", never good
  enough to drive anything.
- **Colours sampled from small areas.** Consistently a few points off per channel — and delivered
  with a confident rationalisation attached ("the standard muted blue of …"). Big flat areas are
  exact; picked colours are not.
- **Full-width vs half-width punctuation.** On the *same image*, one run reproduced `（秒）:`
  correctly and another returned ` (秒) :`. The characters are read; the exact glyph forms drift
  run to run. Don't diff a transcription byte-for-byte against an original.
- **Anything ambiguous in the picture.** Given a table that never said whether a percentage meant
  *remaining* or *consumed*, the run picked one, stated the conclusion flatly, and never mentioned
  that the other reading existed. **Ask the question so it cannot be read two ways**, or ask it to
  list what is ambiguous.
- **Decorative precision.** Answers get dressed with plausible artifacts — a `file://…#L41` line
  anchor was generated for a PNG. Ignore the styling; check the substance.

## Send it small

Legibility is not the constraint — cost is. The same UI at 3600×2400 took roughly twice the
transcript steps of the 900×600 version (and three `view_file` calls instead of one) for the
identical answer, while a 495×330 downscale still read 6px text correctly. **Downscale before
sending.** A dense code screenshot is the expensive case: ~12k ctx and over two minutes.

---

Method, per-arm results and the exact numbers:
`G:\workshopG\gemini-subagent\docs\multimodal-notes.md`.
Measured on `gemini-3.8-flash-medium` only — flash answered everything, so heavier lines were
never compared.
