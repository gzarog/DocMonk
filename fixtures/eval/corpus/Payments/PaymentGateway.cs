using System.Net.Http;

namespace Shop.Payments
{
    /// <summary>Thin client for the card payment provider.</summary>
    public class PaymentGateway
    {
        private readonly HttpClient _http;

        public PaymentGateway(HttpClient http)
        {
            _http = http;
        }

        /// <summary>Captures an authorized payment.</summary>
        public Capture CaptureAsync(string authorizationId, decimal amount)
        {
            var request = new HttpRequestMessage(HttpMethod.Post, $"captures/{authorizationId}");
            request.Headers.Add("X-Request-Source", "shop");
            return Capture.FromResponse(_http.Send(request), amount);
        }

        /// <summary>Voids an authorization that was never captured.</summary>
        public void Void(string authorizationId)
        {
            _http.Send(new HttpRequestMessage(HttpMethod.Delete, $"authorizations/{authorizationId}"));
        }
    }
}
