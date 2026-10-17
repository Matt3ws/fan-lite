# Fonts

Embedded into `fan-lite.exe`. All under the SIL Open Font License 1.1 (licence texts here).
These are the trimmed (Basic Latin, Latin-1, a few punctuation marks) copies that ship with
[peripheral-lite](https://github.com/Skyttehehe/peripheral-lite), so both apps look the same.
Sources: [IBM Plex](https://github.com/IBM/plex) and
[Space Grotesk](https://github.com/google/fonts/tree/main/ofl/spacegrotesk).

| File | Font |
|---|---|
| `LLSans-Regular/Medium/SemiBold.ttf` | IBM Plex Sans, trimmed and renamed "LL Sans" (Reserved Font Name "Plex") |
| `LLMono-Regular/SemiBold.ttf` | IBM Plex Mono, trimmed and renamed "LL Mono" |
| `SpaceGrotesk-wght.ttf` | Space Grotesk, variable weight, trimmed |

The trimmed fonts have no arrows, triangles or dots beyond `← ↑ → ↓ · • ×`, so the UI draws
those symbols with the painter instead of using text.
