export interface Coupon {
  code: string;
  percent: number;
  stackable: boolean;
}

export interface Cart {
  items: { sku: string; price: number; qty: number }[];
  coupons: Coupon[];
}

/** Largest combined discount any cart may receive, in percent. */
const MAX_DISCOUNT = 40;

/**
 * Applies a coupon to the cart. Two coupons combine only when both are
 * marked stackable; the combined discount is capped.
 */
export function applyCoupon(cart: Cart, coupon: Coupon): Cart {
  const others = cart.coupons.filter((c) => c.code !== coupon.code);
  if (others.length > 0 && !(coupon.stackable && others.every((c) => c.stackable))) {
    return { ...cart, coupons: [coupon] };
  }
  const total = [...others, coupon].reduce((sum, c) => sum + c.percent, 0);
  if (total > MAX_DISCOUNT) {
    throw new Error(`discount ${total}% exceeds ${MAX_DISCOUNT}%`);
  }
  return { ...cart, coupons: [...others, coupon] };
}

export class CartStore {
  private key = "cart:v2";

  /** Saves the cart in the browser so a reload keeps it. */
  persist(cart: Cart): void {
    window.localStorage.setItem(this.key, JSON.stringify(cart));
  }

  restore(): Cart | null {
    const raw = window.localStorage.getItem(this.key);
    return raw ? (JSON.parse(raw) as Cart) : null;
  }
}
