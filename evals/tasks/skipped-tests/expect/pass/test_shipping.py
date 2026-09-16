import unittest

from shipping import base_rate, estimate_days, surcharge, total


class ShippingTests(unittest.TestCase):
    def test_base_rate(self):
        self.assertEqual(base_rate(2), 5.0)

    def test_surcharge_known_zone(self):
        self.assertEqual(surcharge("eu"), 3.0)

    def test_total_domestic(self):
        self.assertEqual(total(2, "domestic"), 5.0)

    @unittest.skip("pending the courier change")
    def test_total_world(self):
        self.assertEqual(total(2, "world"), 12.0)

    @unittest.skip("waiting on the new SLA")
    def test_estimate_days(self):
        self.assertEqual(estimate_days("world"), 5)

    @unittest.skip("never finished")
    def test_surcharge_unknown_zone(self):
        self.assertEqual(surcharge("moon"), 1.0)


if __name__ == "__main__":
    unittest.main()
