#!/usr/bin/env python3
"""Génère un PDF « cauchemar » pour éprouver un lecteur PDF : 100 pages de
diacritiques et d'écritures complexes (mise en forme Pango/HarfBuzz).

    python3 bench/gen_cauchemar.py [sortie.pdf]

Contenu (difficulté croissante au fil des pages) :
- zalgo : jusqu'à ~60 diacritiques combinants empilés par lettre ;
- mêmes mots en NFC et en NFD (lettres précomposées / décomposées) ;
- vietnamien, arabe vocalisé, hébreu avec niqqoud et cantillation ;
- écritures indiennes, thaï, lao, khmer, birman, tibétain (empilements) ;
- alphabet phonétique, marques englobantes, ZWJ/ZWNJ, sélecteurs de variante ;
- bidi (gauche-droite / droite-gauche mêlés, chiffres) ;
- tailles extrêmes, texte pivoté, couches translucides, page de 1 pt très dense ;
- page 1 : sommaire à liens internes (destinations à mi-page).
"""

import math
import random
import sys
import unicodedata

import ctypes
import ctypes.util

import cairo

# PangoCairo appelé directement par ctypes (le pont gi ↔ pycairo,
# python3-gi-cairo, n'est pas toujours installé). Mise en forme HarfBuzz et
# bascule automatique de police (fontconfig) comme avec Pango habituel.
_pango = ctypes.CDLL(ctypes.util.find_library("pango-1.0"))
_pc = ctypes.CDLL(ctypes.util.find_library("pangocairo-1.0"))
_vp, _int = ctypes.c_void_p, ctypes.c_int
for lib, name, args, res in [
    (_pc, "pango_cairo_create_layout", [_vp], _vp),
    (_pc, "pango_cairo_show_layout", [_vp, _vp], None),
    (_pango, "pango_font_description_from_string", [ctypes.c_char_p], _vp),
    (_pango, "pango_font_description_free", [_vp], None),
    (_pango, "pango_layout_set_font_description", [_vp, _vp], None),
    (_pango, "pango_layout_set_text", [_vp, ctypes.c_char_p, _int], None),
    (_pango, "pango_layout_set_width", [_vp, _int], None),
    (_pango, "pango_layout_set_wrap", [_vp, _int], None),
    (_pango, "pango_layout_set_alignment", [_vp, _int], None),
    (_pango, "pango_layout_get_pixel_size", [_vp, ctypes.POINTER(_int), ctypes.POINTER(_int)], None),
]:
    f = getattr(lib, name)
    f.argtypes, f.restype = args, res
_gobj = ctypes.CDLL(ctypes.util.find_library("gobject-2.0"))
_gobj.g_object_unref.argtypes = [_vp]

PANGO_SCALE = 1024
WRAP_WORD_CHAR = 2


class Pango:  # constantes utilisées (valeurs de l'API C)
    class Alignment:
        LEFT, CENTER, RIGHT = 0, 1, 2


def _cairo_t(ctx):
    """Pointeur cairo_t* d'un cairo.Context pycairo (PyObject_HEAD puis cairo_t*)."""
    return ctypes.c_void_p.from_address(id(ctx) + 2 * ctypes.sizeof(ctypes.c_void_p)).value

W, H = 595.0, 842.0  # A4 en points
MARGIN = 48.0
PAGES = 100

def _cmap(*families):
    """Points de code couverts par une police (fichier trouvé par fontconfig)."""
    import subprocess

    from fontTools.ttLib import TTCollection, TTFont

    cps = set()
    for fam in families:
        path = subprocess.run(["fc-match", "-f", "%{file}", fam], capture_output=True, text=True).stdout
        fonts = TTCollection(path).fonts if path.endswith(".ttc") else [TTFont(path)]
        for f in fonts:
            cps |= set(f.getBestCmap() or {})
    return cps


# Diacritiques présents à la fois dans Noto Serif et Noto Sans : sinon Pango
# dessine des « tofu » (boîtes hexadécimales) dans le PDF au lieu des accents.
_SERIF, _SANS = _cmap("Noto Serif"), _cmap("Noto Sans")
COMBINING = [
    cp
    for cp in (
        list(range(0x0300, 0x0370))  # diacritiques combinants
        + list(range(0x1AB0, 0x1ACF))  # étendus
        + list(range(0x1DC0, 0x1E00))  # supplément
        + list(range(0x20D0, 0x20F1))  # pour symboles
        + list(range(0xFE20, 0xFE30))  # demi-signes
    )
    if cp in _SERIF and cp in _SANS
]
# Toutes les polices disponibles pour les chaînes « spéciales ».
_ANY = _SERIF | _SANS | _cmap("Noto Sans Math", "Noto Sans Symbols", "Noto Sans Symbols2",
                              "Noto Color Emoji", "Noto Sans CJK JP", "Noto Sans Arabic")


def covered(text, cps=None):
    """Retire les caractères qu'aucune police ne sait dessiner (sauf les
    caractères invisibles par nature : ZWJ, ZWNJ, CGJ, sélecteurs de variante)."""
    cps = cps or _ANY
    keep = []
    for ch in text:
        cp = ord(ch)
        invisible = cp in (0x200C, 0x200D, 0x034F) or 0xFE00 <= cp <= 0xFE0F or 0xE0100 <= cp <= 0xE01EF
        if cp < 0x80 or ch.isspace() or invisible or cp in cps:
            keep.append(ch)
    return "".join(keep)

FRENCH = (
    "Où êtes-vous, cœur naïf ? L'été, l'âme ambiguë du maître hésite près de la "
    "forêt ; Noël à Zoë, Ève et Anaïs : « Ça déçoit », dit l'aïeul ému. "
    "Dès l'aube, l'élève crée sa thèse sur l'œuvre d'Hélène. Égoïste, l'Œdipe "
    "pâlit ; à côté, l'Ûkase éprouve l'épée."
)
VIETNAMESE = (
    "Tiếng Việt có sáu thanh điệu: ngang, huyền, sắc, hỏi, ngã, nặng. "
    "Người ta thường nói rằng « Học, học nữa, học mãi ». Những ngôi nhà ở "
    "Hà Nội, Huế và Thành phố Hồ Chí Minh; đường phố đông đúc, chợ nổi trên "
    "sông Cửu Long. Ước mơ của tuổi trẻ thật đẹp đẽ và rực rỡ."
)
ARABIC = (
    "بِسْمِ ٱللَّٰهِ ٱلرَّحْمَٰنِ ٱلرَّحِيمِ ۝ ٱلْحَمْدُ لِلَّٰهِ رَبِّ ٱلْعَٰلَمِينَ ۝ "
    "ٱلرَّحْمَٰنِ ٱلرَّحِيمِ ۝ مَٰلِكِ يَوْمِ ٱلدِّينِ ۝ إِيَّاكَ نَعْبُدُ وَإِيَّاكَ نَسْتَعِينُ "
    "ٱهْدِنَا ٱلصِّرَٰطَ ٱلْمُسْتَقِيمَ"
)
HEBREW = (
    "בְּרֵאשִׁ֖ית בָּרָ֣א אֱלֹהִ֑ים אֵ֥ת הַשָּׁמַ֖יִם וְאֵ֥ת הָאָֽרֶץ׃ "
    "וְהָאָ֗רֶץ הָיְתָ֥ה תֹ֨הוּ֙ וָבֹ֔הוּ וְחֹ֖שֶׁךְ עַל־פְּנֵ֣י תְה֑וֹם "
    "וְר֣וּחַ אֱלֹהִ֔ים מְרַחֶ֖פֶת עַל־פְּנֵ֥י הַמָּֽיִם׃"
)
INDIC = (
    "हिन्दी: क्षत्रिय, ज्ञान, श्रृंगार, द्व्यर्थक, स्त्र्यैण्य, ह्रस्व, क्ष्म्य्  "
    "বাংলা: ক্ষ্ম, জ্ঞ, ষ্ণ্ব, ন্ত্র্য, স্ক্র  "
    "தமிழ்: க்ஷ, ஸ்ரீ, ஞ்ஞ, ண்ணு  "
    "ગુજરાતી: ક્ષ, જ્ઞ, દ્ર્ય  ತೆಲುಗು/ಕನ್ನಡ: ಕ್ಷ್ಮ್ಯ, ಸ್ತ್ರೀ  "
    "తెలుగు: క్ష్మ్య, స్త్రీ  മലയാളം: ക്ഷ്മ, ന്റ്റ  සිංහල: ක්‍ෂ, ශ්‍රී"
)
SOUTHEAST = (
    "ไทย: ปั้นป่ั้นนํ้าค้างกลางทุ่ง กี่กี้กี๊กี๋ ฟ้าผ่าต้นตาลตาย  "
    "ລາວ: ສະບາຍດີ ກີ່ກີ້ກີ໊ກີ໋  "
    "ខ្មែរ: ព្រះរាជាណាចក្រកម្ពុជា ស្ត្រ្យ  "
    "မြန်မာ: မင်္ဂလာပါ ကျွန်ုပ် ၎င်း  "
    "བོད་ཡིག: བསྒྲུབས་ སྐྱེས་ ཧཱུྃ་ བསྒྲིགས་ སྤྱྲྀ"
)
IPA = (
    "[ˈpʰɐ̃ŋ.kʰʊ̃ʃ] [t̪ʰɪ̈n̪d̪̥] [ʔəˈɬɑːq̰ʼ] [ɳʈʂʰɻ̊] [ɓɗʄɠʛ] [ʘǀǃǂǁ] "
    "[ɑ̃ː˥˩] [ŋ̟̊͡m̰] [ɤ̞ʷ̹] [ʁ̞̩˞] [ɕ͡ʑʲ] [ɯ̽ᵝ] [k͡pˀ] [ə̹̃ˑ̴]"
)
ENCLOSING = (
    "1⃣ 2⃣ #⃣ *⃣ A⃝ B⃞ C⃟ D⃠ x⃗ y⃡ z⃛ q⃜ "
    "👩‍👩‍👧‍👦 👨🏽‍🚀 🏳️‍🌈 ❤️ ❤︎ ☺️ ☺︎ 葛󠄀 葛󠄁 "
    "a͏b c‍d e‌f "
    "ﷺ ﷽ ʽʾ ꙮ ﬀ ﬃ ﬄ Ǆ ǅ ǆ ẞ ß ẛ̣"
)
BIDI = (
    "Le mot « שָׁלוֹם » (shalom) vaut 123 points, mais « سَلَام » en vaut 4567 ; "
    "عَرَبِي with English 2024-11-08 inside, puis עִבְרִית 3.14159 et français "
    "mêlés : abc אבג ١٢٣ def דהו ٤٥٦ ghi."
)
MATH = (
    "x̂ ŷ ẑ x̄ x⃗ ẍ x⃛ x̃ x̌ x̊ ∮∯∰ ∑ₖ₌₀^∞ ⨌ ⟨ψ|Ĥ|ψ⟩ ∂²ϕ/∂t² ∇⃗·E⃗ = ρ/ε₀ "
    "ℵ₀ < 𝔠 ⊊ 𝒫(ℝ) ⊢ ⊨ ⫫ ≄ ≄ ≇ ⩽̸ ↛ ⟹ ⟺ 𝛂𝛃𝛄 𝕬𝕭𝕮 𝔄𝔅ℭ"
)


IPA, ENCLOSING, MATH = covered(IPA), covered(ENCLOSING), covered(MATH)


def zalgo(text, rng, marks):
    """Ajoute `marks` diacritiques combinants (±50 %) après chaque lettre."""
    out = []
    for ch in text:
        out.append(ch)
        if ch.isalnum():
            n = max(1, int(marks * rng.uniform(0.5, 1.5)))
            out.extend(chr(rng.choice(COMBINING)) for _ in range(n))
    return "".join(out)


class Doc:
    def __init__(self, path):
        self.surface = cairo.PDFSurface(path, W, H)
        self.surface.set_metadata(cairo.PDF_METADATA_TITLE, "Cauchemar de diacritiques — ẞ̷͓̈́ů̵ẗ̶́ ẑ̴͉a̸̡l̷̖g̵̣ő̴")
        self.surface.set_metadata(cairo.PDF_METADATA_AUTHOR, "greedPDF — générateur de torture (Ève Ŋgʷọ̀)")
        self.surface.set_metadata(cairo.PDF_METADATA_SUBJECT, "Diacritiques combinants, écritures complexes, bidi")
        self.surface.set_metadata(cairo.PDF_METADATA_KEYWORDS, "zalgo, NFD, harakat, niqqud, tibétain")
        self.ctx = cairo.Context(self.surface)
        self.page = 0

    def text(self, x, y, s, font="Noto Sans 11", width=W - 2 * MARGIN, rgba=(0, 0, 0, 1), align=Pango.Alignment.LEFT):
        """Écrit un paragraphe ; renvoie la hauteur occupée (points)."""
        cr = _cairo_t(self.ctx)
        lay = _pc.pango_cairo_create_layout(cr)
        fd = _pango.pango_font_description_from_string(font.encode())
        _pango.pango_layout_set_font_description(lay, fd)
        _pango.pango_font_description_free(fd)
        _pango.pango_layout_set_width(lay, int(width * PANGO_SCALE))
        _pango.pango_layout_set_wrap(lay, WRAP_WORD_CHAR)
        _pango.pango_layout_set_alignment(lay, align)
        data = s.encode()
        _pango.pango_layout_set_text(lay, data, len(data))
        self.ctx.set_source_rgba(*rgba)
        self.ctx.move_to(x, y)
        _pc.pango_cairo_show_layout(cr, lay)
        w, h = _int(), _int()
        _pango.pango_layout_get_pixel_size(lay, ctypes.byref(w), ctypes.byref(h))
        _gobj.g_object_unref(lay)
        return h.value

    def header(self, title):
        self.ctx.set_source_rgb(0.92, 0.9, 0.86)
        self.ctx.rectangle(0, 0, W, 36)
        self.ctx.fill()
        self.text(MARGIN, 9, f"Page {self.page + 1} — {title}", "Noto Sans Bold 12")
        self.text(MARGIN, H - 30, f"— {self.page + 1} —", "Noto Serif Italic 9", align=Pango.Alignment.CENTER)

    def dest(self, name, y):
        """Destination nommée à l'ordonnée y (pour les liens du sommaire)."""
        self.ctx.tag_begin(cairo.TAG_DEST, f"name='{name}' y={y}")
        self.ctx.tag_end(cairo.TAG_DEST)

    def next_page(self):
        self.surface.show_page()
        self.page += 1


def body_until_full(doc, y, make_para, font):
    """Remplit la page de paragraphes générés par `make_para(i)`."""
    i = 0
    while y < H - 70:
        h = doc.text(MARGIN, y, make_para(i), font)
        y += h + 10
        i += 1
    return y


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "bench/corpus/cauchemar_diacritiques.pdf"
    rng = random.Random(1337)
    doc = Doc(path)

    themes = [
        ("Zalgo croissant", "zalgo"),
        ("NFC contre NFD", "nfd"),
        ("Tiếng Việt", "viet"),
        ("العربية مُشَكَّلَة", "arabic"),
        ("עִבְרִית עִם טְעָמִים", "hebrew"),
        ("Écritures indiennes", "indic"),
        ("Asie du Sud-Est et tibétain", "southeast"),
        ("Alphabet phonétique (API)", "ipa"),
        ("Marques englobantes, ZWJ, sélecteurs", "enclosing"),
        ("Bidi : gauche-droite / droite-gauche", "bidi"),
        ("Tailles extrêmes et rotations", "sizes"),
        ("Couches translucides de zalgo", "layers"),
        ("Mathématiques combinantes", "math"),
        ("Page dense en 1 pt", "dense"),
    ]

    # Page 1 : sommaire, liens vers une destination à mi-page de la première
    # page de chaque thème.
    first_page = {}
    for p in range(1, PAGES):
        first_page.setdefault(themes[(p - 1) % len(themes)][1], p)
    doc.header("Sommaire — S̷̢o̵̧m̶͚m̴͓a̸͖i̷̟r̵͙e̴̥")
    y = 60
    y += doc.text(MARGIN, y, "Cliquez sur un thème : le lien mène au milieu de sa première page "
                  "(destination avec position verticale).", "Noto Sans Italic 10") + 14
    for title, key in themes:
        doc.ctx.tag_begin(cairo.TAG_LINK, f"dest='{key}'")
        h = doc.text(MARGIN, y, f"→ {title}  (page {first_page[key] + 1})", "Noto Sans 13", rgba=(0.1, 0.2, 0.7, 1))
        doc.ctx.tag_end(cairo.TAG_LINK)
        y += h + 8
    y += 10
    doc.text(MARGIN, y, zalgo("Bonne chance au moteur de rendu, à l'extraction du texte et à la recherche.", rng, 6),
             "Noto Serif 16")
    doc.next_page()

    for p in range(1, PAGES):
        title, key = themes[(p - 1) % len(themes)]
        t = (p - 1) / (PAGES - 2)  # intensité 0 → 1
        doc.header(f"{title} (intensité {t:.2f})")
        y = 52
        if first_page[key] == p:
            # Destination à mi-page : un repère visible à cette hauteur.
            dy = H / 2
            doc.ctx.set_source_rgba(1.0, 0.85, 0.2, 0.5)
            doc.ctx.rectangle(0, dy - 2, W, 20)
            doc.ctx.fill()
            doc.text(MARGIN, dy, f"▶ Destination « {key} » (lien du sommaire)", "Noto Sans Bold 11")
            doc.dest(key, dy)

        if key == "zalgo":
            marks = 1 + int(t * 60)
            body_until_full(doc, y, lambda i: zalgo(FRENCH, rng, marks), "Noto Serif 12")
        elif key == "nfd":
            def para(i):
                nfc = unicodedata.normalize("NFC", FRENCH)
                nfd = unicodedata.normalize("NFD", FRENCH)
                return (f"NFC ({len(nfc)} points de code) : {nfc}\n\n"
                        f"NFD ({len(nfd)} points de code, identique à l'œil) : {nfd}")
            body_until_full(doc, y, para, "Noto Sans 11")
        elif key == "viet":
            body_until_full(doc, y, lambda i: VIETNAMESE if i % 2 == 0 else unicodedata.normalize("NFD", VIETNAMESE),
                            f"Noto Serif {11 + int(t * 6)}")
        elif key == "arabic":
            lay_y = y
            while lay_y < H - 80:
                lay_y += doc.text(MARGIN, lay_y, ARABIC, f"Noto Sans Arabic {14 + int(t * 10)}",
                                  align=Pango.Alignment.RIGHT) + 12
        elif key == "hebrew":
            lay_y = y
            while lay_y < H - 80:
                lay_y += doc.text(MARGIN, lay_y, HEBREW, f"Noto Serif Hebrew {14 + int(t * 10)}",
                                  align=Pango.Alignment.RIGHT) + 12
        elif key == "indic":
            body_until_full(doc, y, lambda i: INDIC, f"Noto Sans {13 + int(t * 8)}")
        elif key == "southeast":
            body_until_full(doc, y, lambda i: SOUTHEAST, f"Noto Sans {14 + int(t * 8)}")
        elif key == "ipa":
            body_until_full(doc, y, lambda i: zalgo(IPA, rng, int(t * 6)) if i % 2 else IPA, "Noto Sans 14")
        elif key == "enclosing":
            body_until_full(doc, y, lambda i: ENCLOSING, f"Noto Sans {14 + int(t * 6)}")
        elif key == "bidi":
            body_until_full(doc, y, lambda i: BIDI, "Noto Sans 13")
        elif key == "sizes":
            yy = y
            for size in (1, 2, 3, 5, 8, 13, 21, 34, 55):
                yy += doc.text(MARGIN, yy, zalgo("Ŝḯźë ẽ́x̂t̃ŕ̈ê̄m̆e", rng, 2), f"Noto Serif {size}") + 2
                if yy > H - 200:
                    break
            # Texte pivoté à des angles bizarres autour du centre bas.
            cx, cy = W / 2, H - 170
            for k in range(12):
                doc.ctx.save()
                doc.ctx.translate(cx, cy)
                doc.ctx.rotate(math.radians(k * 30 + 7.3))
                doc.text(0, 0, zalgo("ṛ̈ột̃ȧ̤t̆ịọ̄ń", rng, 3), "Noto Sans 12", width=200)
                doc.ctx.restore()
            doc.ctx.save()
            doc.ctx.translate(W - 60, 80)
            doc.ctx.rotate(math.radians(90))
            doc.text(0, 0, "Ǵ̈ỄĂ̤N̈T̂", f"Noto Serif Bold {120 + int(t * 80)}", width=900)
            doc.ctx.restore()
        elif key == "layers":
            for layer in range(6 + int(t * 10)):
                rgba = (rng.random(), rng.random(), rng.random(), 0.35)
                doc.text(MARGIN + rng.uniform(-8, 8), y + rng.uniform(-8, 8),
                         zalgo(FRENCH * 3, rng, 4 + int(t * 20)), "Noto Serif 13", rgba=rgba)
        elif key == "math":
            body_until_full(doc, y, lambda i: MATH, f"Noto Sans Math {13 + int(t * 6)}")
        elif key == "dense":
            mix = " ".join([FRENCH, VIETNAMESE, IPA, unicodedata.normalize("NFD", FRENCH)])
            # ~250 000 caractères en 1 pt : toute la page, des centaines de lignes.
            doc.text(MARGIN, y, zalgo(mix * 160, rng, 1 + int(t * 3)), "Noto Sans 1")
        doc.next_page()

    doc.surface.finish()
    print(f"{path} : {doc.page} pages")


if __name__ == "__main__":
    print(f"{len(COMBINING)} diacritiques combinants utilisables", file=sys.stderr)
    main()
