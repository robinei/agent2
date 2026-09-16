"""Shipping cost rules."""


def base_rate(weight_kg):
    return 4.0 + 0.5 * weight_kg


def surcharge(zone):
    return {"domestic": 0.0, "eu": 3.0, "world": 9.5}.get(zone, 0.0)


def total(weight_kg, zone):
    return round(base_rate(weight_kg) + surcharge(zone), 2)


def estimate_days(zone):
    return {"domestic": 1, "eu": 3, "world": 8}.get(zone)
