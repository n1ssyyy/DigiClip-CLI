# Bundled fonts

These files are compiled into the engine (`src/fonts.rs`) and written to the
fonts folder for libass (`<data dir>/fonts`) on first use. A Look names a font
by its **family**, the name in the first column (case does not matter):
`look.captions.font` and `look.headline.font`.

Every font is under the SIL Open Font License 1.1 or the Apache License 2.0.
The licence text of each family is in [`licenses/`](licenses/). One static
TrueType file per family, the heavy weight creators use; a variable font was
used only where it was already bundled (JetBrains Mono, default instance).

| Family (what a Look uses) | File | Weight | Licence | Category | Source |
| --- | --- | --- | --- | --- | --- |
| Anton | `Anton-Regular.ttf` | 400 | OFL-1.1 | display | https://github.com/google/fonts/tree/main/ofl/anton |
| Bebas Neue | `BebasNeue-Regular.ttf` | 400 | OFL-1.1 | display | https://github.com/google/fonts/tree/main/ofl/bebasneue |
| Oswald | `Oswald-Bold.ttf` | 700 | OFL-1.1 | display | https://github.com/googlefonts/OswaldFont/tree/main/fonts/ttf |
| Archivo Black | `ArchivoBlack-Regular.ttf` | 400 | OFL-1.1 | display | https://github.com/google/fonts/tree/main/ofl/archivoblack |
| Lilita One | `LilitaOne-Regular.ttf` | 400 | OFL-1.1 | rounded | https://github.com/google/fonts/tree/main/ofl/lilitaone |
| Bangers | `Bangers-Regular.ttf` | 400 | OFL-1.1 | comic | https://github.com/google/fonts/tree/main/ofl/bangers |
| Luckiest Guy | `LuckiestGuy-Regular.ttf` | 400 | Apache-2.0 | comic | https://github.com/google/fonts/tree/main/apache/luckiestguy |
| Inter Medium | `Inter-Medium.ttf` | 500 | OFL-1.1 | sans | https://github.com/rsms/inter (static Medium) |
| Montserrat ExtraBold | `Montserrat-ExtraBold.ttf` | 800 | OFL-1.1 | sans | https://github.com/JulietaUla/Montserrat/tree/master/fonts/ttf |
| Poppins | `Poppins-Bold.ttf` | 700 | OFL-1.1 | sans | https://github.com/google/fonts/tree/main/ofl/poppins |
| Space Grotesk | `SpaceGrotesk-Bold.ttf` | 700 | OFL-1.1 | sans | https://github.com/floriankarsten/space-grotesk/tree/master/fonts/ttf/static |
| DM Serif Display | `DMSerifDisplay-Regular.ttf` | 400 | OFL-1.1 | serif | https://github.com/google/fonts/tree/main/ofl/dmserifdisplay |
| Permanent Marker | `PermanentMarker-Regular.ttf` | 400 | Apache-2.0 | hand | https://github.com/google/fonts/tree/main/apache/permanentmarker |
| JetBrains Mono | `JetBrainsMono-Variable.ttf` | 400 (variable) | OFL-1.1 | mono | https://github.com/JetBrains/JetBrainsMono |
| Space Mono | `SpaceMono-Bold.ttf` | 700 | OFL-1.1 | mono | https://github.com/google/fonts/tree/main/ofl/spacemono |

Notes

- A bold-style file is used under its family name (Oswald, Poppins, Space
  Grotesk, Space Mono): the style rows ask for the regular weight and libass
  picks the only face of that family, so nothing is synthesised.
- Bebas Neue has capitals only: lower case draws as the same capitals.
- Licence texts: `licenses/<Family>-OFL.txt` (or `-Apache-2.0.txt`), copied
  unchanged from the family's upstream repository.
- To add a font here: put the static `.ttf` in this folder, its licence in
  `licenses/`, a row in this table and in `BUNDLED` (`src/fonts.rs`). The
  `fonts` tests check that the file reads, that its name table says the family
  written in `BUNDLED`, and that a licence and a row exist.
