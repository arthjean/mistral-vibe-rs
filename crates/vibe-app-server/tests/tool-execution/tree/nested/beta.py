def gather(values):
    """Collect the values worth keeping."""
    kept = []
    for value in values:
        if value is None:
            continue
        kept.append(value)
    return kept


def Scatter(values):
    return list(reversed(gather(values)))
