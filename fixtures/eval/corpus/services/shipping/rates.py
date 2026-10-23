"""Shipping rate tables."""

FREE_SHIPPING_THRESHOLD = 60


def shipping_cost(order, zone):
    """Flat rate per zone; orders above the threshold ship free."""
    if order.total >= FREE_SHIPPING_THRESHOLD:
        return 0
    return {"domestic": 5, "eu": 12, "world": 25}[zone]


def delivery_estimate(zone, ordered_on):
    """Business days until delivery for a zone."""
    days = {"domestic": 2, "eu": 4, "world": 9}[zone]
    return ordered_on + days
