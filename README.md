# greedPDF

Lecteur PDF en Rust pour lire et naviguer instantanément. Il précharge **tout**
le document sur tous les cœurs, et garde les pages rendues en RAM, autant que
nécessaire, sans dépasser 85 % de la RAM du système.

Spécification : [`SPEC.md`](SPEC.md) · Mesures et choix du moteur :
[`bench/RESULTS.md`](bench/RESULTS.md)

## Compiler

Prérequis (Ubuntu 24.04) : Rust (`rustup`), `clang`, `libclang-dev`,
`libfontconfig1-dev`, `libxkbcommon-dev`, un pilote Vulkan.

```bash
cargo build --release
./target/release/greedpdf livre.pdf            # option : --page 42
```

## Raccourcis

| Action | Touches |
|---|---|
| Page suivante / précédente | `→` `←`, `PgDown` `PgUp`, `Espace` / `Maj+Espace`, `j` `k`, `l` `h`, molette (page entière visible) |
| Aller à la page *n* | taper le numéro puis `Entrée` (ou `g`, `Ctrl+G`) ; `Échap` annule |
| Début / fin | `Début` / `Fin` |
| Précédent / suivant (historique) | `Alt+←` / `Alt+→`, `Retour arrière` / `Maj+Retour arrière`, boutons latéraux de la souris |
| Lien interne | clic (le curseur devient une main) |
| Zoom | `+` / `-`, `Ctrl+molette` (autour du curseur) ; `z` ajuster à la page, `w` à la largeur, `Ctrl+0` 100 % |
| Défilement (page zoomée) | molette, `↑` `↓`, `Maj+molette` (horizontal), glisser à la souris |
| Volet des miniatures | `F9` (clic sur une miniature pour y aller) |
| Barre d'état / plein écran | `F1` / `F11` |
| Quitter | `q`, `Ctrl+W` |

## Options

| Option | Rôle |
|---|---|
| `--max-ram 85%` ou `--max-ram 12G` | plafond de RAM système utilisée, ou taille maximale du cache |
| `--max-supersample 2` | facteur maximal de sur-échantillonnage (zoom net sans rendu jusqu'à ce facteur) |
| `--threads N` | nombre de workers de rendu (défaut : tous les cœurs logiques) |
| `-v` | affiche les temps de chaque navigation |
| `--bench [--viewport 3840x2091] [--bench-hold N]` | banc de test sans fenêtre : préchargement complet, mémoire, débit |
| `--script "next,goto:31,zoom+,shot:nom,quit"` | rejoue des actions, mesure leur latence et prend des captures (X11), dans `bench/out/shots` |

## Fonctionnement

- **Rendu** : hayro (pur Rust) pour les pages entières ; MuPDF pour les tuiles
  de zoom fort et l'extraction des liens, et en secours si hayro échoue.
- **Préchargement** : page visible d'abord, puis voisines, puis tout le
  document à la taille de l'écran (S = 1), puis tout le document en plus haute
  résolution (facteur S choisi selon la RAM disponible, jusqu'à ×2) pour un
  zoom net et instantané.
- **Mémoire** : un gouverneur surveille la RAM du système toutes les 250 ms ;
  si d'autres programmes en ont besoin, il réduit les pages lointaines (sans
  les re-rendre), puis les libère. Le swap est un signal d'arrêt.

Licence : AGPL-3.0-or-later (MuPDF est sous AGPL).
