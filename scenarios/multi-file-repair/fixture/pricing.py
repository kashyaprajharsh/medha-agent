def subtotal(unit_price, quantity):
    if quantity < 0:
        raise ValueError("quantity must be non-negative")
    return unit_price + quantity
