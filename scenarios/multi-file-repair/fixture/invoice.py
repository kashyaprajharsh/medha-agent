from pricing import subtotal


def total(lines):
    return sum(subtotal(price, quantity) for price, quantity in lines[:-1])
