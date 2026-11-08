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

| Action | Touches / souris |
|---|---|
| Page suivante / précédente | `→` `←`, `PgDown` `PgUp`, `Espace` / `Maj+Espace`, `j` `k`, `l` `h`, molette (page entière visible ; page zoomée : un cran de plus une fois le bas ou le haut atteint) |
| Aller à la page *n* | champ de page de la barre d'outils (clic, numéro, `Entrée`), ou taper le numéro n'importe où (ou `g`, `Ctrl+G`) ; `Échap` annule. **AZERTY** : chiffres avec `Maj` ou pavé numérique (sans `Maj`, la rangée du haut donne `& é " ' ( - …`, et `-` reste le zoom arrière) |
| Début / fin | `Début` / `Fin` |
| Précédent / suivant (historique) | `Alt+←` / `Alt+→`, `Retour arrière` / `Maj+Retour arrière`, boutons latéraux de la souris |
| Lien interne | clic (le curseur devient une main) |
| Zoom | `+` / `-`, `Maj+molette` ou `Ctrl+molette` (autour du curseur) ; `z` ajuster à la page, `w` à la largeur, `Ctrl+0` 100 % |
| Défilement (page zoomée) | molette, `↑` `↓`, **glisser au clic droit** |
| Sélection de texte | **glisser au clic gauche** (curseur texte au survol) ; `Ctrl+A` tout sélectionner, `Ctrl+C` copier, `Échap` désélectionner |
| Menu contextuel | **clic droit** (sans bouger) : copier le texte ; sur une illustration (encadrée en bleu) : copier l'image / enregistrer l'image sous… (l'image seule, résolution d'origine, transparence conservée) ; copier / enregistrer la page comme image ; tout sélectionner |
| Barre d'outils | à gauche : bouton du volet des miniatures ; champ de page (page courante) ; à droite : pivoter ↺ ↻, loupe (recherche), bouton « Options » (imprimer, plein écran, pivoter, volet des miniatures, propriétés) |
| Rechercher (comme evince) | loupe de la barre d'outils ou `Ctrl+F` : barre de recherche, résultats pendant la frappe, liste des résultats dans le volet (clic pour y aller) ; `Entrée` / `Ctrl+G` / `F3` / `↓` suivant, `Maj+Entrée` / `Ctrl+Maj+G` / `Maj+F3` / `↑` précédent ; options « Aa » (respecter la casse) et « Mots entiers » ; accents et diacritiques combinants ignorés (texte décomposé NFD, zalgo) ; `Échap` ferme ; la croix à droite de la barre termine la recherche, ferme le volet et efface le surlignage (on reste sur la page) |
| Pivoter | boutons ↺ ↻ de la barre d'outils, ou `R` (sens horaire), `E` (sens anti-horaire) |
| Volet des miniatures | bouton à gauche de la barre ou `F9` (masqué au démarrage ; clic sur une miniature pour y aller) |
| Barre d'état / plein écran | `F1` / `F11` |
| Quitter | `q`, `Ctrl+W` |

**Impression** : le fichier est envoyé tel quel à l'imprimante par défaut du
système (`lp`, CUPS) ; il n'y a pas encore de boîte de dialogue d'impression.

## Options

| Option | Rôle |
|---|---|
| `--max-ram 85%` ou `--max-ram 12G` | plafond de RAM système utilisée, ou taille maximale du cache |
| `--max-supersample 2` | facteur maximal de sur-échantillonnage (zoom net sans rendu jusqu'à ce facteur) |
| `--threads N` | nombre de workers de rendu (défaut : tous les cœurs logiques) |
| `-v` | affiche les temps de chaque navigation |
| `--bench [--viewport 3840x2091] [--bench-hold N]` | banc de test sans fenêtre : préchargement complet, mémoire, débit |
| `--script "next,goto:31,zoom+,shot:nom,quit"` | rejoue des actions, mesure leur latence et prend des captures (X11), dans `bench/out/shots` ; liste des actions : `greedpdf --help` |

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

## Licence

greedPDF est un logiciel libre : vous pouvez le redistribuer et le modifier
selon les termes de la **GNU Affero General Public License** publiée par la
Free Software Foundation, version 3 ou (à votre choix) toute version
ultérieure. Voir [`LICENSE`](LICENSE).

Ce choix découle de l'utilisation de MuPDF, distribué sous AGPL.
