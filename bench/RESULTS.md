# greedPDF — Résultats de la phase 0 (choix du moteur)

Date : 2026-11-08 · Machine : AMD Ryzen 5 3600 (6 cœurs / 12 threads), 46 Go RAM
Document : `WFRP5_Core_Rulebook_06_10_26.pdf` (94,7 Mo, 379 pages, très illustré,
nombreuses images JPEG 2000) · Rendu à 2160 px de haut (page ajustée sur écran 4K,
≈ 14,9 Mo/page en RGBA) · Build `--release`.

Reproduire : `cargo run --release -p greedpdf-bench -- bench/corpus/<fichier>.pdf`

## Débit de rendu

| Moteur / mode | Workers | Pages/s | Gain | Médiane / page | Livre entier |
|---|---|---|---|---|---|
| MuPDF, crate `mupdf` (contexte de base partagé) | 1 | 5,0 | ×1 | 163 ms | — |
| MuPDF, crate `mupdf` | 12 | 10,1 | ×2,0 | 1 038 ms | — |
| MuPDF, contextes indépendants (FFI directe) | 1 | 5,1 | ×1 | 197 ms | 74 s |
| MuPDF, contextes indépendants | 6 | 16,5 | ×3,3 | 344 ms | 23 s |
| MuPDF, contextes indépendants | 12 | 14,5 | ×2,9 | 766 ms | 26 s |
| MuPDF, 12 **processus** (40 pages chacun) | 12 | ≈ 32 | ×6,5 | ≈ 310 ms | ≈ 12 s (estimé) |
| **hayro** | 1 | 6,6 | ×1 | 155 ms | 57 s |
| **hayro** | 6 | 28,4 | ×4,3 | 207 ms | — |
| **hayro** | 12 | **27,9** | ×4,5 | 418 ms | **13,6 s** |

Le gain maximal réaliste est d'environ ×6 à ×7 (6 cœurs physiques + SMT).

### Pourquoi MuPDF passe mal à l'échelle en threads

1. La crate `mupdf` clone tous les contextes depuis **un seul** contexte de base :
   réserve de ressources (256 Mo) et verrous partagés → forte contention.
2. Avec des contextes indépendants, MuPDF **exige** quand même un verrou global
   `FZ_LOCK_FREETYPE` : OpenJPEG (JPEG 2000), HarfBuzz et FreeType utilisent des
   variables globales (cf. `source/fitz/load-jpx.c`). Sans ce verrou : segfault
   (constaté). Avec, tout décodage JPEG 2000 est sérialisé — et ce livre en est plein.
3. Seul le multi-processus contourne ces globales.

## Fidélité (comparaison pixel à pixel, 379 pages à 1080 px)

`greedpdf-bench --compare <dossier>` : écart moyen global **4,8/255** ; pire page :
2,3 % de pixels très différents (> 64/255). Contrôle visuel des pires pages
(31, 12) : **rendus identiques à l'œil** ; les écarts viennent de l'anti-crénelage
du texte et du ré-échantillonnage des images. Aucune erreur ni panic sur les
deux moteurs.

Dimensions des pages : identiques au millième de point entre les deux moteurs.

## Liens internes

MuPDF : 430 liens internes extraits en **87 ms** pour tout le livre.
hayro : pas d'API de liens.

## Décision

**Rendu : hayro** (pur Rust, threads simples, meilleur débit en threads, rendu
fidèle sur le document de référence).
**Liens : MuPDF**, une seule instance, une fois à l'ouverture (87 ms) — les
dimensions de pages des deux moteurs sont identiques, les coordonnées des liens
sont donc directement utilisables.
**Secours : MuPDF** pour le rendu si hayro échoue sur une page (erreur ou panic)
ou sur un document (ex. PDF chiffré) ; même trait `Backend`.

Pistes d'amélioration ultérieures :
- MuPDF multi-processus (≈ 32 pages/s) si un document lourd l'exige.
- Contribuer à hayro / suivre ses optimisations (son README indique qu'aucun
  travail d'optimisation n'a encore été fait).

## PDF « cauchemar » de diacritiques (`bench/gen_cauchemar.py`)

100 pages générées avec Pango/HarfBuzz (14 Mo, 62 polices embarquées) :
zalgo jusqu'à ~60 diacritiques par lettre, NFC/NFD, vietnamien, arabe
vocalisé, hébreu avec cantillation, écritures indiennes, thaï/khmer/tibétain,
API, marques englobantes et ZWJ, bidi, tailles de 1 à 200 pt, texte pivoté,
couches translucides, pages de ~250 000 caractères en 1 pt ; sommaire à liens
dont les destinations sont à mi-page.

Régénérer : `python3 bench/gen_cauchemar.py bench/corpus/cauchemar_diacritiques.pdf` (~12 s).

| Mesure | Résultat |
|---|---|
| Rendu, 12 threads | hayro 103 pages/s, MuPDF 92 pages/s ; aucune erreur |
| Comparaison MuPDF / hayro (100 pages) | écart moyen 2,6/255 ; pire page 1,95 % de pixels différents (anti-crénelage) |
| Texte extrait (MuPDF) | 3,78 millions de caractères en 3,3 s |
| Liens | 14 liens, destinations avec position verticale (y = 421) : arrivée vérifiée en mode largeur |
| Recherche | « naif » : 136 résultats (NFC **et** NFD) ; « etes » : 59 résultats, y compris dans le zalgo |

Correctif issu de ce test : la recherche ignorait les accents des lettres
précomposées seulement ; elle décompose maintenant chaque caractère (NFD) et
ignore tous les diacritiques combinants (texte NFD, zalgo, vietnamien décomposé).
