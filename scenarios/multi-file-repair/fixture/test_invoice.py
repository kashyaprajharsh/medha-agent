import unittest
from pricing import subtotal
from invoice import total


class InvoiceTests(unittest.TestCase):
    def test_subtotal(self):
        self.assertEqual(subtotal(7, 3), 21)
        self.assertEqual(subtotal(7, 0), 0)
        with self.assertRaises(ValueError):
            subtotal(7, -1)

    def test_all_lines_included(self):
        self.assertEqual(total([(7, 3), (4, 2)]), 29)
        self.assertEqual(total([(2, 3)]), 6)
        self.assertEqual(total([]), 0)
