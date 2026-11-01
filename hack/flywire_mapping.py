#!/usr/bin/env python3
"""Generate data/flywire-783/mapping.json: FlyOps sensory channels -> stimulated FlyWire neurons, and
readout populations -> recorded FlyWire neurons.

The mapping is ENGINEERED by this project. The cell types are real FlyWire v783 annotations, but which types stand
for "looming", "odor-left", ... and which ones FlyOps reads out is a design choice, not biology.

Inputs (see hack/fetch-flywire.sh):
  neuron annotations  flyconnectome/flywire_annotations v2.1.0, Supplemental_file1_neuron_annotations.tsv: the
                      version published with Schlegel et al., Nature 634:139 (2024), which is licensed CC BY 4.0
  neuron list         philshiu/Drosophila_brain_model, Completeness_783.csv (defines the simulator's neuron set)

Usage: python3 -I hack/flywire_mapping.py ANNOTATIONS.tsv Completeness_783.csv > data/flywire-783/mapping.json
Only the Python standard library is used.
"""
import csv
import json
import sys

ORN_GLOMERULI = ("DM1", "DM2", "DM4", "VA2")  # food-odour glomeruli (attractive odours)


def typ(r):
    """Effective cell type: v2.1.0 leaves cell_type empty when it equals the hemibrain type."""
    return r["cell_type"] or r["hemibrain_type"]


# name -> predicate over an annotation row. Kept small and named after cell types so it can be reviewed.
CHANNELS = {
    # looming-sensitive visual projection neurons (feed the giant-fibre escape pathway)
    "looming": lambda r: typ(r) in ("LPLC2", "LC4"),
    "strong-looming": lambda r: typ(r) in ("LPLC2", "LC4", "LPLC1"),
    # ocellar photoreceptor pathway: abrupt light changes
    "abrupt": lambda r: r["cell_class"] == "ocellar",
    # olfactory receptor neurons of a few food-odour glomeruli, per side
    "odor-left": lambda r: typ(r) in tuple(f"ORN_{g}" for g in ORN_GLOMERULI) and r["side"] == "left",
    "odor-right": lambda r: typ(r) in tuple(f"ORN_{g}" for g in ORN_GLOMERULI) and r["side"] == "right",
    # thermosensory receptor neurons
    "temperature": lambda r: r["cell_class"] == "thermosensory",
    # antennal mechanosensory (Johnston's organ C/D/E: wind, gravity, vibration)
    "touch": lambda r: typ(r).startswith(("JO-C", "JO-D", "JO-E")),
}

POPULATIONS = {
    # escape / startle descending neurons, including the giant fibre DNp01
    "example-descending-population": lambda r: typ(r) in ("DNp01", "DNp02", "DNp04", "DNp06", "DNp11"),
    # mushroom body output neurons
    "example-mushroom-body-population": lambda r: r["cell_class"] == "MBON",
    # central complex steering outputs
    "example-central-complex-population": lambda r: typ(r) in ("PFL1", "PFL2", "PFL3"),
    # uniglomerular projection neurons downstream of the odour channels' glomeruli
    "example-antennal-lobe-population": lambda r: r["cell_class"] == "ALPN" and typ(r).split("_")[0] in ORN_GLOMERULI,
    # centrifugal neurons projecting from the central brain back into the optic lobe
    "example-optic-lobe-population": lambda r: r["super_class"] == "visual_centrifugal",
}


POPULATION_REF_HZ = {
    "example-descending-population": 250.0,  # looming 0.3 -> ~80 Hz, 1.0 -> ~180 Hz
    "example-antennal-lobe-population": 320.0,  # odour -> ~260-290 Hz
    "example-mushroom-body-population": 160.0,  # odour -> ~140 Hz
    "example-optic-lobe-population": 10.0,  # abrupt 0.3 -> ~2 Hz, 1.0 -> ~8 Hz
    "example-central-complex-population": 20.0,  # not driven by any channel in this model (stays ~0)
}


def main(ann_path: str, comp_path: str) -> None:
    with open(comp_path, newline="") as f:
        rdr = csv.reader(f)
        next(rdr)
        known = {int(row[0]) for row in rdr if row}
    with open(ann_path, newline="") as f:
        rows = [r for r in csv.DictReader(f, delimiter="\t") if int(r["root_id"]) in known]

    def select(spec):
        out = {}
        for name, pred in spec.items():
            ids = sorted({int(r["root_id"]) for r in rows if pred(r)})
            if not ids:
                sys.exit(f"{name}: no neurons matched")
            out[name] = ids
        return out

    mapping = {
        "description": (
            "FlyWire FAFB v783 (138,639 neurons; FlyWire connectivity CC BY 4.0) via philshiu/Drosophila_brain_model @91bdd1e (MIT). "
            "Channel/population selections are FlyOps-engineered from flyconnectome/flywire_annotations v2.1.0 cell types (Schlegel et al. 2024, CC BY 4.0) "
            "(hack/flywire_mapping.py). Cite Dorkenwald et al. 2024, Schlegel et al. 2024, Shiu et al. 2024."
        ),
        "neurons_file": "Completeness_783.csv",
        "connections_file": "Connectivity_783.parquet",
        "max_input_rate_hz": 150.0,
        "activity_ref_hz": 20.0,
        # Mean rate that reads out as activity 1.0, per population: calibrated so the population's own pathway
        # spans roughly 0.3..0.9 between weak (0.3) and full (1.0) stimulus intensity on this model.
        "population_ref_hz": POPULATION_REF_HZ,
        "channels": select(CHANNELS),
        "populations": select(POPULATIONS),
    }
    json.dump(mapping, sys.stdout, indent=1)
    sys.stdout.write("\n")
    for kind in ("channels", "populations"):
        for k, v in mapping[kind].items():
            print(f"{kind[:-1]:>10} {k:<36} {len(v):>4} neurons", file=sys.stderr)


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
