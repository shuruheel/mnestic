"""SearchOp filter semantics — a faithful port of the reference behavior in
`langgraph.store.memory` (`_compare_values` / `_apply_operator`), so filters
behave identically to `InMemoryStore` / the Postgres store: top-level keys into
the value dict, nested matching via nested dicts, `$eq/$ne/$gt/$gte/$lt/$lte`
operators, JSONB-style list equality.

Reimplemented (not imported) because upstream keeps these private.
"""

from __future__ import annotations

from typing import Any, Dict, Optional


def item_matches(value: Dict[str, Any], filter_: Optional[Dict[str, Any]]) -> bool:
    if not filter_:
        return True
    return all(compare_values(value.get(key), fv) for key, fv in filter_.items())


def compare_values(item_value: Any, filter_value: Any) -> bool:
    if isinstance(filter_value, dict):
        if any(k.startswith("$") for k in filter_value):
            return all(
                _apply_operator(item_value, op_key, op_value)
                for op_key, op_value in filter_value.items()
            )
        if not isinstance(item_value, dict):
            return False
        return all(compare_values(item_value.get(k), v) for k, v in filter_value.items())
    elif isinstance(filter_value, (list, tuple)):
        return (
            isinstance(item_value, (list, tuple))
            and len(item_value) == len(filter_value)
            and all(compare_values(iv, fv) for iv, fv in zip(item_value, filter_value))
        )
    else:
        return item_value == filter_value


def _apply_operator(value: Any, operator: str, op_value: Any) -> bool:
    if operator == "$eq":
        return value == op_value
    elif operator == "$gt":
        return float(value) > float(op_value)
    elif operator == "$gte":
        return float(value) >= float(op_value)
    elif operator == "$lt":
        return float(value) < float(op_value)
    elif operator == "$lte":
        return float(value) <= float(op_value)
    elif operator == "$ne":
        return value != op_value
    else:
        raise ValueError(f"Unsupported operator: {operator}")
