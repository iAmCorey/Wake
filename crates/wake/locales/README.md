# Language packs

Wake's UI text is English by default. A language pack is one flat JSON file that
maps **the English string itself** to its translation:

```json
{
  "$name": "简体中文",
  "All Sessions": "全部会话",
  "{} min ago": "{} 分钟前"
}
```

- The key is the exact English text as it appears in the app. Anything the pack
  doesn't cover falls back to English, so a partial translation is fine and
  shipping one never breaks the UI.
- `$name` is the language's own name for itself; it's what the picker in
  **Settings → General → Language** shows.
- The file name is the BCP 47 tag: `zh-Hans.json`, `ja.json`, `de.json`,
  `pt-BR.json`. Wake matches the system language against it (exact tag, then
  tag prefix, then the primary language code — with the script subtag honoured
  for Chinese, so `zh-TW` never falls into a Simplified pack).
- English is a pack too (`en.json`, which carries only `$name` because the keys
  are already English). There is no special case for it: dropping your own
  `en.json` into the config directory rewrites the English wording.

## Adding a language

**Without rebuilding** — drop the file into Wake's config directory and restart:

| Platform | Path |
| --- | --- |
| macOS | `~/Library/Application Support/wake/locales/<tag>.json` |
| Linux | `~/.config/wake/locales/<tag>.json` |
| Windows | `%APPDATA%\wake\locales\<tag>.json` |

A file there also overrides a bundled pack with the same tag, so you can correct
a translation on your own machine without touching the app.

**As a pull request** — put the file in this directory and add one row to the
`BUNDLED` table at the top of `crates/wake/src/i18n.rs` (tag, the language's own
name, and the `include_str!`). That's the only code change needed; everything
else is data.

## Placeholders

`{}` slots are filled left to right with the values the English string carries.
When the translation needs a different word order, use explicit indices:

```json
{ "{} to {}": "把 {1} 存到了 {0}" }
```

`{0}` is the first value, `{1}` the second. Referring to an index the English
string doesn't provide leaves the literal `{2}` visible in the UI — a unit test
(`translations_never_reference_missing_arguments`) fails the build on that, so
run `cargo test -p wake` after editing a pack.

Strings starting with `%` are [chrono] date formats rather than prose — translate
them into the date order your language reads in (`"%b %-d"` → `"%-m月%-d日"`),
keeping the `%` directives intact.

[chrono]: https://docs.rs/chrono/latest/chrono/format/strftime/index.html
