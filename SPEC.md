# greedPDF — Spécification

> Lecteur PDF en Rust, optimisé pour la lecture et la navigation instantanée.
> Principe : **précharger tout le document** en utilisant **tous les cœurs** et
> **autant de RAM que nécessaire, jusqu'à 85 % de la RAM système**.

Version : 0.1 (2026-11-08) — statut : brouillon

---

## 1. Objectifs

| # | Objectif |
|---|----------|
| O1 | Tourner une page déjà en cache en **moins d'une frame** (< 16 ms à 60 Hz). |
| O2 | Rendre tout le document en arrière-plan avec **tous les cœurs logiques**, sans jamais bloquer l'interface. |
| O3 | Utiliser la RAM comme cache, **au besoin seulement**, plafonnée à **85 % de RAM système utilisée**, et sans jamais déclencher de swap. |
| O3b | Zoom **net et instantané** sans aucun rendu jusqu'au facteur S permis par la RAM (§4.4). |
| O4 | Navigation minimale mais rapide : zoom, page ±1, aller à la page *n*, liens internes, historique précédent/suivant. |
| O5 | 100 % Rust côté application (le moteur de rendu peut être une bibliothèque C appelée via FFI). |

### Non-objectifs (v1)

Pas d'annotations, de formulaires, de recherche texte, de sélection/copie, de
signets, d'onglets, d'impression, ni d'édition. Pas de mode défilement continu
(une page à la fois). Pas de support Windows/macOS garanti (Linux d'abord, sans
choix qui l'interdise plus tard).

---

## 2. Fonctionnalités

### 2.1 Navigation

| Action | Description |
|--------|-------------|
| **Page +1 / Page −1** | Tourner la page. N'alimente **pas** l'historique. |
| **Aller à la page *n*** | Saisie d'un numéro puis Entrée. Alimente l'historique. |
| **Clic sur lien interne** | Saute à la destination (page + position verticale si fournie). Alimente l'historique. Le curseur devient une main au survol d'un lien. |
| **Précédent / Suivant** | Historique façon navigateur : revenir à la position d'avant un saut (lien ou goto), puis y retourner. |
| **Zoom** | Zoom +/−, ajuster à la page, ajuster à la largeur, 100 %. Quand la page dépasse la fenêtre, déplacement à la molette, aux flèches `↑` `↓`, par `Maj+molette` (horizontal) ou par glisser-déposer. |

Règles du zoom :
- Le zoom (ou le mode « ajuster ») est **conservé** quand on change de page.
- Après page ±1, l'affichage se place en **haut** de la nouvelle page (page −1 : en haut aussi, pour lire dans l'ordre).

Règles de l'historique :
- Une entrée = (page, décalage vertical, zoom).
- Un saut (lien ou goto) empile la position **courante** puis efface la pile « suivant ».
- Les liens externes (URL) sont ignorés en v1 (option : ouvrir avec `xdg-open`).
- Le premier et le dernier élément d'une pile sont des butées (pas de bouclage).

### 2.2 Raccourcis clavier / souris

| Touche | Action |
|--------|--------|
| `→`, `PgDown`, `Espace`, `l`, `j` | Page +1 |
| `←`, `PgUp`, `Maj+Espace`, `h`, `k` | Page −1 |
| `Home` / `End` | Première / dernière page (alimente l'historique) |
| un chiffre (ou `g`, `Ctrl+G`), puis les chiffres, puis `Entrée` | Aller à la page *n*. Taper un chiffre ouvre directement le champ. Dans le champ : `Backspace` efface, `Échap` annule ; un numéro hors bornes est ramené à la première/dernière page. |
| `Alt+←`, `Backspace` (hors champ goto), bouton latéral « Précédent » de la souris | Précédent (historique) |
| `Alt+→`, `Maj+Backspace`, bouton latéral « Suivant » de la souris | Suivant (historique) |
| `+` / `=`, `Ctrl+molette ↑` | Zoom avant (×1,25) |
| `-`, `Ctrl+molette ↓` | Zoom arrière (÷1,25) |
| `z` | Ajuster à la page |
| `w` | Ajuster à la largeur |
| `Ctrl+0` | Zoom 100 % |
| molette / `↑` `↓` | Page entièrement visible : la molette tourne la page (±1). Page zoomée : défilement vertical ; une fois le bas (ou le haut) atteint, un cran de plus passe à la page suivante, affichée en haut (ou précédente, affichée en bas). *Modifié à la demande : la v0 ne tournait pas la page au bord.* |
| `Maj+molette` | Défilement horizontal (page zoomée) |

Les chiffres sont réservés au goto : aucune autre action n'utilise une touche
chiffre seule (d'où `z` et `Ctrl+0` pour le zoom).
| clic gauche sur lien | Suivre le lien |
| `q`, `Ctrl+W` | Quitter |
| `F9` | Afficher / masquer le volet des miniatures |
| `F11` | Plein écran |

Tous les raccourcis sont centralisés dans une table (reconfigurable en v2).

### 2.3 Interface

- **Volet des miniatures** (ajouté en cours de développement, à la demande) :
  à gauche, comme dans evince. Une miniature par page, produite gratuitement
  en réduisant la première image rendue de la page ; numéro sous chaque
  miniature ; la page courante est surlignée et le volet la suit ; clic sur une
  miniature = saut (alimente l'historique) ; la molette fait défiler le volet.

- Une fenêtre, la page centrée sur un fond neutre.
- Une barre d'état discrète (masquable avec `F1`) : `page / total`, zoom %,
  progression du préchargement (`1234/1500 pages`), RAM utilisée par le cache.
- Pendant la saisie d'un goto, un petit champ s'affiche au-dessus de la barre d'état.
- Lancement : `greedpdf fichier.pdf [--page N]`.

---

### 2.4 Fonctionnalités ajoutées après le MVP (`fonctionnalités.md`)

| Demande | Réalisation |
|---|---|
| Bouton « Options » : impression, plein écran, pivoter (`R` horaire, `E` anti-horaire), propriétés copiables | Bouton en haut à droite, menu déroulant. Impression : envoi du fichier à l'imprimante par défaut (`lp`, CUPS), sans boîte de dialogue. Rotation par quarts de tour faite par le GPU (aucun re-rendu des images en cache ; les tuiles et les liens suivent). Propriétés : fichier, dossier, taille, pages, métadonnées PDF (MuPDF), format de page, liens, texte, état du préchargement ; bouton « Copier ». |
| Menu clic droit : copier l'image, tout sélectionner | Clic droit sans déplacement. « Copier l'image de la page » copie l'image L0 (orientée comme à l'écran) ; « Copier le texte » apparaît s'il y a une sélection. |
| Molette : monter, descendre | Défilement de la page zoomée ; page ±1 quand la page entière est visible, et aussi en continuant à tourner la molette au bord d'une page zoomée. |
| `Maj+molette` : zoom | Zoom autour du curseur (`Ctrl+molette` aussi). Le défilement horizontal passe par le glisser au clic droit. |
| Glisser au clic droit : déplacer la vue | Oui (auparavant au clic gauche). |
| Clic gauche : sélection du texte | Glisser au clic gauche ; texte de toutes les pages extrait en arrière-plan par MuPDF (WFRP5 : 1,2 million de caractères en 1,5 s) ; sélection dans l'ordre de lecture, surlignée en bleu ; `Ctrl+C`, `Ctrl+A`, `Échap`. Un simple clic suit toujours les liens. |
| Volet masqué par défaut | Oui (`F9`). |
| Barre en haut : champ de page (page courante par défaut), bouton du volet à gauche | Barre d'outils au-dessus des pages : bouton du volet (trois traits, bleu quand ouvert) ; champ de page qui affiche la page courante — un clic la sélectionne, les chiffres tapés la remplacent, `Entrée` y va, `Échap` annule ; « / N » ; bouton « Options » à droite. Taper des chiffres n'importe où remplit ce champ (remplace l'ancienne fenêtre « Aller à la page »). |
| Recherche (Ctrl+F) calquée sur evince | Loupe dans la barre d'outils → barre de recherche sous celle-ci : champ, précédent / suivant, « Aa » (casse), « Mots entiers », compteur « n sur N ». Recherche pendant la frappe sur le texte de tout le document (déjà en mémoire), saut au premier résultat à partir de la page courante. Le volet affiche la liste des résultats (page, extrait, occurrence surlignée) ; clic = aller au résultat. Occurrences surlignées sur la page (courante en orange). Raccourcis : Entrée / Ctrl+G / F3 / ↓, Maj+Entrée / Ctrl+Maj+G / Maj+F3 / ↑, Échap ferme et rétablit le volet ; la croix (à droite de la barre de recherche) termine la recherche, ferme le volet et efface le surlignage, sans changer de page. Accents et majuscules ignorés par défaut ; les mots coupés en fin de ligne sont trouvés. |

## 3. Choix du moteur PDF

### 3.1 Candidats

| Moteur | Crate | Nature | Licence | Multi-threading |
|--------|-------|--------|---------|-----------------|
| **MuPDF** | `mupdf` | C via FFI (MuPDF compilé avec la crate) | **AGPL-3.0** | MuPDF supporte le multi-thread avec un contexte par thread et un document par thread → rendu parallèle. *Gestion des verrous par la crate et statut Send/Sync de ses types : à vérifier en spike.* |
| **PDFium** | `pdfium-render` | C++ via FFI (binaire `libpdfium.so` à fournir) | BSD/Apache | PDFium n'est **pas** thread-safe ; la crate sérialise les appels (à confirmer en spike). Pour exploiter les 12 cœurs, il faut alors **un processus par worker** (comme Chrome). |
| **hayro** | `hayro` | Pur Rust (2025) | MIT/Apache (à vérifier) | A priori parallélisable directement (un rendu par thread) ; maturité et couverture des PDF exotiques **à vérifier**. |
| pdf-rs, lopdf | `pdf`, `lopdf` | Pur Rust | MIT | **Analyse et modification de PDF ; pas de rendu exploitable** (pdf-rs n'a qu'un rendu expérimental et incomplet). Hors course pour l'affichage. |

### 3.2 Décision (après la phase 0 — détails dans `bench/RESULTS.md`)

> Recommandation initiale : MuPDF par défaut, hayro en alternative. **Les mesures
> l'ont inversée.**

- **Rendu : hayro**, en threads. Sur le livre WFRP5 (379 pages, 12 threads) :
  27,9 pages/s, livre entier préchargé en 13,6 s, rendu identique à l'œil à
  MuPDF, aucune erreur.
- **Liens : MuPDF**, une instance unique, une fois à l'ouverture (430 liens en
  87 ms). Les dimensions de pages des deux moteurs sont identiques.
- **Secours : MuPDF** pour le rendu des pages ou documents que hayro ne sait pas
  traiter (erreur, panic, PDF chiffré).
- MuPDF en threads plafonne à ~16 pages/s : il impose un verrou global sur ses
  décodeurs JPEG 2000 / FreeType / HarfBuzz (variables globales). Seul le
  multi-processus contourne cette limite (~32 pages/s) : gardé en réserve.
- **PDFium** : non évalué (même problème de multi-processus que MuPDF).

### 3.3 Phase 0 — spike de décision (obligatoire avant le reste)

Petit binaire `greedpdf-bench` qui, pour chaque backend :
1. ouvre le document dans N workers (N = 1, 2, 4, 8, 12) ;
2. rend toutes les pages à une résolution fixe (ex. 1000 px de large) ;
3. mesure : pages/s, temps de la 1re page, RAM par worker, passage à l'échelle
   (12 workers ≈ ×10 par rapport à 1 ?) ;
4. vérifie visuellement quelques pages (export PNG) pour la fidélité.

Critère : on retient le backend qui a le meilleur débit à 12 workers **avec un
rendu correct** sur tout le corpus. En cas d'égalité, préférer celui qui passe à
l'échelle en **threads** (plus simple qu'en processus).

Corpus de test suggéré (déjà présent sur la machine) :
- un livre de JDR volumineux et illustré (ex. le livre d'*Eternal Lies* sur le NAS) ;
- les cartes `~/MA_Gloucester_*_geo.pdf` (pages immenses → test des tuiles et du zoom) ;
- un PDF texte de plusieurs centaines de pages avec table des matières liée ;
- un PDF scanné (une grosse image par page).

### 3.4 Abstraction

L'application ne dépend que d'un trait ; le backend est choisi à la compilation
(feature Cargo) :

```rust
pub trait Backend: Sized {
    /// Ouvre une instance indépendante du document (une par worker).
    fn open(bytes: Arc<[u8]>) -> Result<Self>;
    fn page_count(&self) -> usize;
    /// Taille de la page en points PDF (sans rendu).
    fn page_size(&self, page: usize) -> Result<SizePt>;
    /// Rend une page ou une région (tuile) à l'échelle demandée, en RGBA8.
    fn render(&mut self, req: &RenderRequest, out: &mut PixelBuf) -> Result<()>;
    /// Liens d'une page, destinations internes déjà résolues.
    fn links(&mut self, page: usize) -> Result<Vec<Link>>;
}
```

Le pool de workers possède **une instance par worker**. Un backend multi-processus
(PDFium) se branche en implémentant le même trait côté processus enfant, avec
transfert des pixels par mémoire partagée (`memfd` + `mmap`).

---

## 4. Architecture

```
┌──────────────────────── processus greedpdf ────────────────────────┐
│                                                                    │
│  Thread UI (winit + wgpu)                                          │
│   ├─ boucle d'événements, raccourcis, historique                   │
│   ├─ textures GPU des pages visibles (+ voisines)                  │
│   └─ envoie la « position d'intérêt » au Scheduler                 │
│              │ ▲                                                   │
│     demandes │ │ pages prêtes (canal)                              │
│              ▼ │                                                   │
│  Scheduler (file de priorité, annulation, budget mémoire)          │
│              │                                                     │
│     ┌────────┼────────┬────────┐                                   │
│  Worker 1  Worker 2  …  Worker N   (N = nb cœurs logiques)         │
│  [Backend] [Backend]    [Backend]  priorité OS basse (nice +5)     │
│              │                                                     │
│              ▼                                                     │
│  PageCache (bitmaps RGBA en RAM, LRU pondéré par distance)         │
│              ▲                                                     │
│  MemoryGovernor (sonde /proc/meminfo + swap, toutes les 250 ms)    │
└────────────────────────────────────────────────────────────────────┘
```

### 4.1 Chargement

1. `mmap` du fichier (`memmap2`), copie dans un `Arc<[u8]>` partagé par tous les
   workers (ou partage direct du mmap si le backend l'accepte).
2. Le worker 0 ouvre le document **et rend immédiatement la page demandée**
   (page 1 ou `--page N`) : la fenêtre l'affiche dès que possible.
3. En parallèle : récupération de la taille et des liens de **toutes** les pages
   (bien moins coûteux qu'un rendu ; permet la mise en page et la priorité des
   cibles de liens). Les liens de la page visible sont extraits en premier.
4. Les autres workers ouvrent leur instance et commencent le préchargement.

### 4.2 Ordonnancement (Scheduler)

File de priorité ; une tâche = (page, niveau de résolution, tuile éventuelle).
Ordre, recalculé à chaque changement de page ou de zoom :

1. **Page visible** au zoom courant (tuiles visibles en premier si zoom fort).
2. **Voisines ±1, ±2, …, ±k** au zoom courant (k = 3, en privilégiant le sens de lecture : +1 avant −1).
3. **Cibles des liens** de la page visible, au zoom courant (pour que les clics soient instantanés).
4. **Tout le reste du document** en L0 à S = 1, en spirale à partir de la page courante.
5. **Montée en résolution** : une fois tout le document en L0 à S = 1, re-rendu de
   tout le document au facteur S cible (§4.4), toujours en spirale à partir de la
   page courante.

Règles :
- Une tâche périmée (zoom changé, page lointaine) qui n'a pas commencé est
  **annulée** ; celle en cours se termine mais son résultat n'est mis en cache que
  s'il reste utile.
- Les workers tournent à priorité OS basse : le thread UI n'est jamais privé de CPU.
- **Réserve interactive** (ajoutée après mesure) : les étapes 6 et 7 n'occupent
  jamais plus de N − 2 workers. Un rendu en cours ne peut pas être interrompu ;
  sans réserve, une action de l'utilisateur pendant le préchargement attendait
  la fin d'un rendu d'arrière-plan (~0,5 s).
- Le préchargement s'arrête quand tout est en cache **ou** quand le budget mémoire
  est atteint (pas d'activité CPU inutile une fois le document chargé).

### 4.3 Niveaux de résolution

Définition : **taille « ajustée »** = page entière ajustée à la fenêtre, en pixels
physiques de l'écran. Dans cette section, « zoom ×1 » désigne cette taille
(mode « ajuster à la page »).

| Niveau | Contenu | Portée |
|--------|---------|--------|
| **L0 (base sur-échantillonnée)** | Page entière à la taille ajustée × **S** (facteur de sur-échantillonnage, §4.4) | **Tout le document** |
| **L1 (zoom courant)** | *Non implémenté : les tuiles L2 couvrent aussi ce cas.* | — |
| **L2 (tuiles)** | Tuiles de 1024×1024 px au palier de zoom courant, **rendues par MuPDF** (2 à 7 % du coût d'une page entière, contre 15 à 50 % pour hayro qui réinterprète toute la page). Seulement quand le zoom dépasse S. | Tuiles visibles + couronne ; haut des pages ±1 |

Changement de zoom :
- **Zoom ≤ S** : aucun rendu. L'image L0 est réduite par le GPU (mipmaps, filtrage
  trilinéaire) : **nette et instantanée**.
- **Zoom > S** : on affiche **tout de suite** L0 agrandi par le GPU (flou un
  instant), puis on le remplace par le rendu L1/L2 net dès qu'il arrive. Le rendu
  de la page visible est découpé en tuiles réparties sur **tous les workers** pour
  arriver le plus vite possible.
- Le zoom avance par paliers de ×1,25. Les rendus L1/L2 d'un palier restent en
  cache (dans la limite du budget) : revenir à un palier déjà vu est instantané.
- Un rendu L1/L2 n'est lancé qu'après ~100 ms sans nouveau zoom (anti-rebond).

Si la fenêtre est redimensionnée, L0 est **progressivement** re-rendu ; l'ancien
L0 reste affiché (mis à l'échelle) en attendant.

### 4.4 Sur-échantillonnage adaptatif (« greed »)

Principe : plus il y a de RAM disponible, plus L0 est rendu en haute résolution,
et plus la plage de zoom sans aucun rendu est large.

Calcul du facteur S, une fois connues la taille de toutes les pages (§4.1) :

```
octets(S)  = Σ_pages  largeur_ajustée × hauteur_ajustée × 4 × S²
S_cible    = plus grand palier de {1 ; 1,25 ; 1,5 ; 1,75 ; 2}
             tel que octets(S) ≤ 80 % du budget du cache (§5.1)
```

- Les 20 % restants du budget sont réservés à L1/L2 et aux paliers de zoom.
- **S max par défaut : 2** (`--max-supersample`). Au-delà, une page 4K dépasse
  ~119 Mo (×3) et son envoi au GPU ne tient plus dans une frame ; un S plus élevé
  nécessiterait d'envoyer L0 au GPU par tuiles (v2).
- Déroulement : tout le document est d'abord rendu à S = 1 (navigation complète le
  plus tôt possible), puis re-rendu à S_cible. Chaque page passe de S = 1 à
  S_cible en remplaçant son ancienne image (la mémoire de la version S = 1 est
  libérée aussitôt).
- **Dégradation plutôt qu'éviction** : en cas de pression mémoire, les pages les
  plus éloignées sont **réduites sur CPU** de S_cible à S = 1 (quelques ms par page,
  sans re-rendu PDF) avant d'envisager de les supprimer. S_cible est recalculé ;
  quand la RAM se libère, la montée en résolution reprend.

Exemple sur la machine de dev (écrans 4K, page A4 ajustée ≈ 1528 × 2160 px,
≈ 13,2 Mo à S = 1, ~31 Go de budget dont 80 % ≈ 25 Go pour L0) :

| Document | S_cible | RAM de L0 | Zoom net sans rendu jusqu'à |
|----------|---------|-----------|-----------------------------|
| 300 pages | 2 (max) | 13,2 × 4 × 300 ≈ 15,8 Go | ×2 |
| 500 pages | 1,75 | 13,2 × 3,06 × 500 ≈ 20,2 Go | ×1,75 |
| 800 pages | 1,5 | 13,2 × 2,25 × 800 ≈ 23,8 Go | ×1,5 |
| 2 000 pages | 1 | 13,2 × 2 000 ≈ 26,4 Go | ×1 |

S = 1 est un plancher : si même S = 1 dépasse les 80 % (cas des 2 000 pages), L0
peut utiliser tout le budget, et L1/L2 se contentent de ce qui reste (au minimum
les entrées verrouillées, §5.1).

---

## 5. Gestion de la mémoire

### 5.1 Définition du plafond

> Le plafond porte sur la **RAM utilisée par tout le système**, pas sur ce que
> greedPDF alloue :
>
> `utilisée_système = MemTotal − MemAvailable` (lu dans `/proc/meminfo`)
>
> **Invariant : `utilisée_système ≤ 85 % × MemTotal`.**

greedPDF ne prend **que ce dont il a besoin** : si tout le document tient dans
2 Go, il utilise 2 Go et s'arrête là. Le plafond n'est qu'une limite haute.

Budget du cache, recalculé toutes les 250 ms :

```
marge      = 0.85 × MemTotal − (MemTotal − MemAvailable)
budget     = cache_actuel + marge
```

Seule exception à l'invariant : les entrées verrouillées (page visible et ±1,
quelques dizaines de Mo) sont toujours conservées, sinon rien ne s'afficherait.

- `marge > 0` → le cache peut grossir de `marge`.
- `marge < 0` (une autre application a pris de la RAM) → **éviction** immédiate de
  `|marge|` + 5 % d'hystérésis.
- **Le swap est un signal d'arrêt** : si `SwapTotal − SwapFree` augmente pendant
  que le cache grossit, gel de la croissance et éviction de 10 %. Un cache en swap
  est plus lent qu'un re-rendu.
- Optionnel : surveiller `/proc/pressure/memory` (PSI) pour réagir plus tôt.

Paramètres en ligne de commande : `--max-ram 85%` (ou `--max-ram 12G`),
`--threads N` (défaut : tous les cœurs logiques).

### 5.2 Ordre de grandeur (machine actuelle : 46 Go, ~8 Go déjà utilisés)

Formule : `octets = largeur_px × hauteur_px × 4` (RGBA). Le poids du PDF sur
disque n'entre pas en compte : une page de texte de 50 Ko pèse autant rendue
qu'une page illustrée.

Page A4 ajustée à la hauteur de l'écran :

| Écran | Taille rendue | Taille d'une page | 1 000 pages | Pages tenant dans ~31 Go |
|-------|---------------|-------------------|-------------|--------------------------|
| 1080p | 764 × 1080 px | ≈ 3,3 Mo | ≈ 3,3 Go | ≈ 9 400 |
| 1440p | 1000 × 1414 px | ≈ 5,7 Mo | ≈ 5,7 Go | ≈ 5 500 |
| **4K (écrans de la machine de dev)** | 1528 × 2160 px | **≈ 13,2 Mo** | ≈ 13,2 Go | **≈ 2 350** |
| 4K, zoom ×2 | 3056 × 4320 px | ≈ 52,8 Mo | ≈ 52,8 Go — **impossible** | ≈ 590 |

Plafond : 0,85 × 46 ≈ 39 Go, dont ~31 Go disponibles pour le cache (8 Go déjà
utilisés par le système). Sur la machine de dev (deux écrans 4K), un livre de
2 000 pages tient donc entièrement en L0.

Pistes pour réduire (v2) : stockage en RGB sur 3 octets (−25 %), en niveaux de
gris pour les pages sans couleur (−75 %), compression LZ4 des pages lointaines
(×2 à ×4 sur du texte).

La mémoire croît comme le **carré du zoom** : c'est pourquoi seul L0 couvre tout
le document (à un facteur S adapté à la RAM, §4.4), et L1/L2 seulement une
fenêtre autour de la page courante.

### 5.3 Éviction

Coût d'une entrée = taille en octets. Score d'éviction (on évince le plus grand) :

```
score = distance_en_pages_à_la_page_courante × poids_niveau
poids_niveau : L2 = 4, L1 = 2, L0 = 1   (on sacrifie d'abord le haut zoom lointain)
```

Ordre de libération de la mémoire :
1. L2 puis L1 lointains (score décroissant) ;
2. **réduction** des pages L0 lointaines de S_cible à S = 1 (§4.4) ;
3. suppression des pages L0 lointaines (S = 1).

Les entrées « verrouillées » (page visible, ±1) ne sont jamais évincées. Une page
évincée ou réduite redevient une tâche dans la file du Scheduler.

### 5.4 Allocation

- Allocateur `mimalloc` ; buffers de pixels recyclés via un pool par taille pour
  éviter la fragmentation.
- Option v2 : compression LZ4 des pages L0 lointaines (×2 à ×4 de capacité sur du
  texte, décompression < 2 ms).

---

## 6. Rendu à l'écran

- **winit** (fenêtre, événements) + **wgpu** (Vulkan sur la RTX 2070 SUPER).
- Les bitmaps vivent en RAM ; seules la page visible et les pages ±2 sont envoyées
  en textures GPU (5 pages × 53 Mo × 1,33 pour les mipmaps ≈ 350 Mo sur les 8 Go de
  VRAM de la RTX 2070 SUPER).
- Textures avec **mipmaps** générés sur le GPU à l'envoi : la réduction de L0
  (zoom ≤ S) reste nette, sans scintillement du texte. Agrandissement transitoire
  (zoom > S) en filtrage linéaire.
- Envoi d'une page L0 à S = 2 sur écran 4K (≈ 53 Mo) : ≈ 5 ms en PCIe 3.0 x16,
  compatible avec une frame. Les pages ±1 sont envoyées à l'avance pour que le
  changement de page n'ait aucun envoi à faire.
- Texte de l'interface (barre d'état, champ goto) : `glyphon` ou rendu de chiffres
  minimal ; pas de framework d'interface complet.
- Redessin **à la demande** uniquement (pas de boucle à 60 fps quand rien ne bouge).

Alternative si le développement doit aller plus vite : `eframe`/`egui` (backend
wgpu), au prix d'un peu plus de latence et de dépendances.

---

## 7. Critères d'acceptation (mesurés sur la machine de dev, 12 cœurs / 46 Go)

| # | Critère | Cible |
|---|---------|-------|
| A1 | Affichage de la 1re page après lancement | < 300 ms (doc de 500 pages, moteur choisi) |
| A2 | Page ±1 quand la page est en cache | < 16 ms entre la touche et l'image |
| A3 | Clic sur lien interne vers une page en cache | < 16 ms |
| A4 | Goto vers une page en cache | < 16 ms |
| A5 | Changement de zoom au-delà de S : image (floue) affichée | < 16 ms ; rendu net < 200 ms pour une page A4 |
| A5b | Changement de zoom ≤ S (document entièrement en L0 à S_cible) | image **nette** en < 16 ms, aucun rendu PDF déclenché |
| A6 | Préchargement complet L0 à S = 1 d'un livre de 500 pages | à fixer après le spike (estimation : < 15 s) ; la montée à S_cible prend environ S² fois plus |
| A7 | Utilisation CPU pendant le préchargement | ≥ 90 % des cœurs |
| A8 | Interface réactive pendant le préchargement | aucune frame > 50 ms |
| A9 | RAM système jamais au-dessus de 85 % ; aucun swap provoqué par greedPDF | vérifié par un test de charge (autre processus qui alloue 10 Go en cours de lecture) |
| A10 | CPU au repos une fois le document chargé | ≈ 0 % |

Mode `greedpdf --bench fichier.pdf` : charge tout, affiche pages/s, temps de
la 1re page, pic de RAM, puis quitte.

### 7.1 État mesuré (MVP, 2026-11-08 — WFRP5, 379 pages, écran 4K, machine chargée)

Mesures du mode `--script` : temps entre l'action et la soumission de la frame
au GPU (l'écran 30 Hz ajoute jusqu'à 33 ms d'affichage). Les actions `key:`,
`wheel:`, `ctrlwheel:`, `sidewheel:`, `drag:` et `mouselink:` passent par les
vrais gestionnaires du clavier et de la souris (seuls les événements du
système de fenêtrage sont simulés).

| # | Mesuré | Verdict |
|---|--------|---------|
| A1 | 264 à 423 ms selon la charge de la machine | ≈ objectif (dépassé quand la machine est chargée) |
| A2 | 1,3 à 2,5 ms | ✅ |
| A3 | 7 à 16 ms (page en cache) ; jusqu'à ~49 ms quand la page cible est à S = 1,75–2 (envoi de 42–60 Mo au GPU + mipmaps) ; ~560 ms si la page n'est pas encore rendue | ✅ / ⚠️ selon S |
| A4 | 9 à 44 ms (même cause : taille de l'envoi au GPU) | ✅ / ⚠️ selon S |
| A5 | flou affiché en 0,8 à 11 ms ✅ ; net en 0,3 à 1,4 s ❌ | ❌ net : chaque worker MuPDF décode les images JPEG 2000 de la page sous le verrou global de MuPDF. Piste : display list partagée (contextes clonés) |
| A5b | 1,3 à 3,1 ms, aucun rendu | ✅ |
| A6 | tout à S = 1 en 19,5 à 21,9 s ; à S = 1,75 en 46,5 à 47,5 s (379 pages) | mesuré (objectif à fixer) |
| A7 | 77 % des 12 threads | ❌ en partie voulu (réserve interactive : 83 % au maximum) |
| A8 | non mesuré directement ; aucune action > 60 ms pour une page en cache pendant le préchargement | partiel |
| A9 | 10 Go pris par un autre programme : RAM système max 83,8 %, S 1,75 → 1,5, RSS 19,8 → 18,5 Go, retour à S = 1,75 après libération | ✅ |
| A10 | 1 % d'un cœur | ✅ |

---

## 8. Organisation du code

```
greedPDF/
├── Cargo.toml                 (workspace)
├── SPEC.md, README.md
├── crates/
│   ├── greedpdf/              binaire
│   │   ├── main.rs            options, mode --bench
│   │   ├── app.rs             interface : vue, navigation, liens, historique, volet, --script
│   │   ├── engine.rs          workers, priorités, cache, gouverneur mémoire, miniatures
│   │   ├── gpu.rs             wgpu : textures, mipmaps, quads
│   │   ├── layout.rs          géométrie partagée (ajustement, paliers, tuiles)
│   │   ├── text.rs            texte (ab_glyph, police du système)
│   │   └── mem.rs             /proc/meminfo, /proc/vmstat, RSS
│   ├── greedpdf-core/         trait Backend, types communs, réduction d'image
│   ├── backend-hayro/         rendu pur Rust (pages entières)
│   ├── backend-mupdf/         FFI directe mupdf-sys (tuiles, liens, secours)
│   └── greedpdf-bench/        banc de test des moteurs (+ examples/ : tuiles, liens, tailles)
└── bench/                     RESULTS.md ; corpus/ et out/ non versionnés
```

Crates principales : `winit`, `wgpu`, `hayro`, `mupdf-sys`, `memmap2`,
`mimalloc`, `parking_lot`, `ab_glyph`, `clap`, `anyhow`, `self_cell`.

---

## 9. Jalons

| Jalon | Contenu | Livrable |
|-------|---------|----------|
| **M0** ✅ | Spike moteurs (§3.3) | Tableau de benchmarks + choix du backend |
| **M1** ✅ | Fenêtre winit/wgpu qui affiche une page rendue | `greedpdf doc.pdf --page 12` |
| **M2** ✅ | Pool de workers + Scheduler + PageCache (sans plafond) | Préchargement complet, page ±1 instantanée |
| **M3** ✅ | MemoryGovernor (plafond 85 %, swap, éviction) | Test de charge A9 réussi |
| **M4** ✅ | Goto, Home/End, barre d'état | A4 |
| **M5** ✅ | Liens internes + historique précédent/suivant | A3 |
| **M6** ✅ (A5 net non atteint) | Zoom (sur-échantillonnage adaptatif S, mipmaps, L1, tuiles L2, zoom GPU transitoire, défilement) | A5, A5b |
| **M7** ✅ (+ volet des miniatures, mode --script) | Mode `--bench`, finitions, raccourcis complets | Tous les critères §7 |

---

## 10. Prérequis (Ubuntu 24.04)

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Compilation de MuPDF via la crate `mupdf` (bindgen → libclang)
sudo apt install clang libclang-dev libfontconfig1-dev
# Fenêtre winit (clavier X11/Wayland)
sudo apt install libxkbcommon-dev libwayland-dev libx11-dev
```

Déjà présents : gcc, make, cmake, pkg-config, git, curl, pilote NVIDIA 580 avec
Vulkan (`nvidia_icd.json`).
Pour le backend PDFium uniquement : télécharger un binaire `libpdfium.so`
précompilé (projet *pdfium-binaries*).

---

## 11. Risques

| Risque | Impact | Parade |
|--------|--------|--------|
| Types de la crate `mupdf` non `Send` | Pas de pool de threads simple | Créer chaque instance **dans** son thread (jamais transférée) ; sinon multi-processus |
| hayro rend mal certains PDF | Fidélité | Garder MuPDF par défaut |
| Swap malgré le plafond (autres applications) | Lenteur générale | Surveillance du swap + éviction agressive (§5.1) |
| PDF énormes (cartes, scans 600 dpi) | Rendu L0 lent, RAM | Tuiles, plafond de taille L0, priorité à la page visible |
| Licence AGPL (MuPDF) | Bloquant seulement en cas de distribution fermée | Backend interchangeable (trait) |
| PDF chiffrés / corrompus | Échec d'ouverture | Message d'erreur clair ; mot de passe en v2 |

---

## 12. Hypothèses validées

- **Précédent / Suivant** = historique de navigation (retour avant un lien ou un goto).
- **Page +1 / −1** = tourner les pages.
- **85 %** = plafond de RAM système utilisée ; greedPDF n'occupe que ce dont le document a besoin.

## 13. Questions ouvertes

- Affichage double page (livres) : v1 ou v2 ?
- Mode sombre (inversion des couleurs de la page) : utile pour la lecture ?
- Mémoriser la dernière page lue par fichier à la réouverture ?
