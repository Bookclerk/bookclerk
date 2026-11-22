"""Rebuilds the model plugin's test graph: `tiny.onnx`.

The graph has the shape of a published embedding model after its token
tables were turned into graph inputs (see `docs/internals/model-packs.md`):
it takes token positions, an attention mask and the rows of two tables
(8 signed bytes and one scale per token), and gives the scaled rows per
token (`last_hidden_state`) and their mean (`sentence_embedding`). The
tests compute the same numbers in Rust and compare.

Usage:
    python3 make_fixture.py

Needs `pip install onnx`.
"""

from __future__ import annotations

import pathlib

import onnx
from onnx import TensorProto, helper

WIDTH = 8


TABLES = ("tok.weight", "tok.scale")


def main() -> None:
    """Writes `tiny.onnx` beside this script."""
    nodes = [
        helper.make_node("Gather", ["tok.weight", "input_ids"], ["rows"], axis=0),
        helper.make_node("Cast", ["rows"], ["rows_f"], to=TensorProto.FLOAT),
        helper.make_node("Gather", ["tok.scale", "input_ids"], ["scales"], axis=0),
        helper.make_node("Mul", ["rows_f", "scales"], ["scaled"]),
        helper.make_node("Cast", ["attention_mask"], ["mask_f"], to=TensorProto.FLOAT),
        helper.make_node("Unsqueeze", ["mask_f", "last_axis"], ["mask_3d"]),
        helper.make_node("Mul", ["scaled", "mask_3d"], ["last_hidden_state"]),
        helper.make_node(
            "ReduceMean", ["last_hidden_state", "token_axis"], ["sentence_embedding"], keepdims=0
        ),
    ]
    graph = helper.make_graph(
        nodes,
        "tiny",
        [
            helper.make_tensor_value_info("input_ids", TensorProto.INT64, [1, "tokens"]),
            helper.make_tensor_value_info("attention_mask", TensorProto.INT64, [1, "tokens"]),
            helper.make_tensor_value_info("tok.weight", TensorProto.INT8, ["rows", WIDTH]),
            helper.make_tensor_value_info("tok.scale", TensorProto.FLOAT, ["rows", 1]),
        ],
        [
            helper.make_tensor_value_info(
                "last_hidden_state", TensorProto.FLOAT, [1, "tokens", WIDTH]
            ),
            helper.make_tensor_value_info("sentence_embedding", TensorProto.FLOAT, [1, WIDTH]),
        ],
        initializer=[
            helper.make_tensor("last_axis", TensorProto.INT64, [1], [2]),
            helper.make_tensor("token_axis", TensorProto.INT64, [1], [1]),
        ],
    )
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 18)])
    model.ir_version = 9
    onnx.checker.check_model(model)
    # The pack builder leaves a table input's shape out (some operators
    # refuse a declared one), so the fixture does too, after the check.
    for graph_input in model.graph.input:
        if graph_input.name in TABLES:
            graph_input.type.tensor_type.ClearField("shape")
    out = pathlib.Path(__file__).resolve().parent / "tiny.onnx"
    out.write_bytes(model.SerializeToString())


if __name__ == "__main__":
    main()
