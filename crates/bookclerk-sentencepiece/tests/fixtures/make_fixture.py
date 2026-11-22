"""Rebuilds the tokenizer test fixtures: `<name>.spm.fb` and `<name>.ids.json`.

Trains a tiny SentencePiece BPE model with byte fallback (the kind the
embedding models use) on the sentences below, converts it to the Lite
FlatBuffers format with upstream's `spm_to_fb`, and records the ids full
SentencePiece gives for the probe texts. The Rust tests check that the
Lite runtime gives the same ids.

There are two fixtures: `tiny` puts a word boundary before every text, as
bekko's tokenizer does; `plain` does not and keeps every space, as Gemma's
does.

Usage:
    python3 make_fixture.py /path/to/spm_to_fb

Needs `pip install sentencepiece` and `spm_to_fb` built from the commit
pinned in `third_party/sentencepiece-lite/README.md`
(`scripts/model-packs/build-spm-to-fb.sh <directory>`).
"""

from __future__ import annotations

import json
import pathlib
import subprocess
import sys
import tempfile

import sentencepiece as spm

CORPUS = [
    "A detective in Victorian London follows a trail of forged letters.",
    "Dragons guard a stolen ring beneath the northern mountains.",
    "The lighthouse keeper writes to a daughter he has never met.",
    "Two sisters inherit a bookshop and a secret in the cellar.",
    "A starship crew wakes to find the captain missing.",
    "The river remembers every name it has carried to the sea.",
    "Ein Detektiv im viktorianischen London folgt einer Spur gefälschter Briefe.",
    "Drachen bewachen einen gestohlenen Ring unter den nördlichen Bergen.",
    "Die Schwestern erben eine Buchhandlung und ein Geheimnis im Keller.",
    "Der Leuchtturmwärter schreibt an eine Tochter, die er nie getroffen hat.",
    "Un détective dans le Londres victorien suit une piste de lettres falsifiées.",
    "Des dragons gardent un anneau volé sous les montagnes du nord.",
    "Deux sœurs héritent d'une librairie et d'un secret dans la cave.",
    "Le gardien du phare écrit à une fille qu'il n'a jamais rencontrée.",
    "Un detective en el Londres victoriano sigue un rastro de cartas falsificadas.",
    "Los dragones custodian un anillo robado bajo las montañas del norte.",
    "Dos hermanas heredan una librería y un secreto en el sótano.",
    "El farero escribe a una hija que nunca ha conocido.",
    "Un investigatore nella Londra vittoriana segue una traccia di lettere false.",
    "Um detetive na Londres vitoriana segue um rasto de cartas falsificadas.",
    "ヴィクトリア朝のロンドンで探偵が偽造された手紙の跡を追う。",
    "竜は北の山の下で盗まれた指輪を守っている。",
    "二人の姉妹が本屋と地下室の秘密を受け継ぐ。",
]

PROBES = [
    "",
    "A detective in Victorian London",
    "Dragons and a stolen ring",
    "Die Schwestern erben ein Geheimnis",
    "Deux sœurs héritent d'un secret",
    "El farero escribe cartas",
    "探偵が指輪を守っている。",
    "  leading and  double  spaces ",
    "tabs\tand\nnew lines",
    "Unseen: żółć, Ελληνικά, 🙂",
    "x" * 300,
]


def write_fixture(here: pathlib.Path, spm_to_fb: str, name: str, add_dummy_prefix: bool) -> None:
    """Trains one fixture model and writes its two files beside this script.

    Args:
        here: The fixtures directory.
        spm_to_fb: Path of upstream's converter.
        name: The fixture's file stem.
        add_dummy_prefix: Whether the model puts a word boundary before a
            text.
    """
    with tempfile.TemporaryDirectory() as tmp:
        work = pathlib.Path(tmp)
        corpus = work / "corpus.txt"
        corpus.write_text("\n".join(CORPUS * 4) + "\n", encoding="utf-8")
        spm.SentencePieceTrainer.train(
            input=str(corpus),
            model_prefix=str(work / name),
            model_type="bpe",
            vocab_size=640,
            byte_fallback=True,
            character_coverage=0.98,
            normalization_rule_name="identity",
            add_dummy_prefix=add_dummy_prefix,
            # Gemma's model keeps every space; the default drops leading,
            # trailing and doubled ones.
            remove_extra_whitespaces=add_dummy_prefix,
            pad_id=0,
            eos_id=1,
            bos_id=2,
            unk_id=3,
            num_threads=1,
            minloglevel=2,
        )
        subprocess.run(
            [
                spm_to_fb,
                f"--model={work / f'{name}.model'}",
                f"--output={here / f'{name}.spm.fb'}",
            ],
            check=True,
        )
        reference = spm.SentencePieceProcessor(model_file=str(work / f"{name}.model"))
        record = {
            "vocab_size": reference.GetPieceSize(),
            "special": {
                "padding": reference.pad_id(),
                "end": reference.eos_id(),
                "begin": reference.bos_id(),
                "unknown": reference.unk_id(),
            },
            "texts": [{"text": text, "ids": reference.encode(text)} for text in PROBES],
        }
    (here / f"{name}.ids.json").write_text(
        json.dumps(record, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )


def main() -> None:
    """Writes both fixtures beside this script."""
    here = pathlib.Path(__file__).resolve().parent
    write_fixture(here, sys.argv[1], "tiny", add_dummy_prefix=True)
    write_fixture(here, sys.argv[1], "plain", add_dummy_prefix=False)


if __name__ == "__main__":
    main()
