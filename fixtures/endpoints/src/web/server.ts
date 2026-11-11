// Express routes: an application, a router mounted under a prefix, and
// calls that only look like routes.
import express, { Request, Response, Router } from "express";
import axios from "axios";

const app = express();
const orders = Router();
const admin = express.Router();

const NOT_A_ROUTE = "/api/web/not-a-route";
const cache = new Map<string, string>();

export function listOrders(req: Request, res: Response) {
  res.json([]);
}

export function getOrder(req: Request, res: Response) {
  res.json({ id: req.params.orderId });
}

function createOrder(req: Request, res: Response) {
  res.status(201).end();
}

function requireAdmin(req: Request, res: Response, next: () => void) {
  next();
}

async function purgeOrders(req: Request, res: Response) {
  // A client call with a URL and a second argument is not a route.
  await axios.post("/api/web/remote/purge", { all: true });
  await axios.get("/api/web/remote/status", { timeout: 1000 });
  cache.get("/api/web/cached/value");
  res.status(204).end();
}

app.get("/health/live", (req, res) => {
  res.send("ok");
});

orders.get("/", listOrders);
orders.get("/:orderId", getOrder);
orders.post("/", requireAdmin, createOrder);
admin.delete("/orders", requireAdmin, purgeOrders);
admin.all("/ping", (req, res) => res.end());

orders.use("/admin", admin);
app.use("/api/web/orders", orders);

export default app;
