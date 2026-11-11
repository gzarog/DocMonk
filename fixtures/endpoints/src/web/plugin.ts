// Fastify routes: shorthand methods, the full route declaration, and a
// plugin registered under a prefix.
import Fastify, { FastifyInstance } from "fastify";

const server = Fastify();

async function getInvoice(request: unknown, reply: unknown) {
  return { ok: true };
}

async function replaceInvoice(request: unknown, reply: unknown) {
  return { ok: true };
}

async function invoiceRoutes(fastify: FastifyInstance) {
  fastify.get("/:invoiceId", getInvoice);
  fastify.route({
    method: ["PUT", "PATCH"],
    url: "/:invoiceId",
    handler: replaceInvoice,
  });
  fastify.post("/", { schema: {} }, async (request, reply) => {
    return { created: true };
  });
}

server.get("/ready", async () => ({ ready: true }));
server.register(invoiceRoutes, { prefix: "/api/web/invoices" });

export default server;
