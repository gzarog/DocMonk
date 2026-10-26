namespace Shop.Contracts
{
    /// The event published when an order is placed.
    public class OrderPlaced
    {
        public OrderPlaced(string id)
        {
            Id = id;
        }

        public string Id { get; }
    }
}
