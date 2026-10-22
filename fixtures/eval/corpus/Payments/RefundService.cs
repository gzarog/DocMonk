using System;
using System.Net.Http;

namespace Shop.Payments
{
    public class RefundService
    {
        private const decimal ManagerApprovalThreshold = 500m;
        private readonly HttpClient _http;

        public RefundService(HttpClient http)
        {
            _http = http;
        }

        /// <summary>
        /// Issues a refund through the payment provider. Large refunds wait
        /// for a manager; every request carries an idempotency key so a
        /// retried call never refunds twice.
        /// </summary>
        public Refund Issue(Order order, decimal amount, string reason)
        {
            if (amount > order.Paid)
            {
                throw new InvalidOperationException("refund exceeds the captured amount");
            }
            if (amount > ManagerApprovalThreshold)
            {
                return Refund.PendingApproval(order.Id, amount, reason);
            }
            var request = new HttpRequestMessage(HttpMethod.Post, "refunds");
            request.Headers.Add("X-Idempotency-Key", $"{order.Id}:{amount}");
            var response = _http.Send(request);
            return Refund.FromResponse(response);
        }

        public bool CanRefund(Order order)
        {
            return order.CapturedAt > DateTime.UtcNow.AddDays(-180);
        }
    }
}
