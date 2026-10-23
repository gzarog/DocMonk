import { Cart } from "./cart";

/** Sums line items before discounts. */
export function subtotal(cart: Cart): number {
  return cart.items.reduce((sum, i) => sum + i.price * i.qty, 0);
}

/** Applies every coupon on the cart to the subtotal. */
export function totalWithDiscounts(cart: Cart): number {
  const percent = cart.coupons.reduce((sum, c) => sum + c.percent, 0);
  return Math.round(subtotal(cart) * (100 - percent)) / 100;
}

export class CheckoutSession {
  constructor(private readonly cart: Cart) {}

  /** Starts payment with the provider and returns the redirect URL. */
  async start(provider: string): Promise<string> {
    const res = await fetch(`/api/checkout/${provider}`, {
      method: "POST",
      body: JSON.stringify({ total: totalWithDiscounts(this.cart) }),
    });
    return (await res.json()).redirect;
  }
}
