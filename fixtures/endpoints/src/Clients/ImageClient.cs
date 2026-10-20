namespace Shop.Clients
{
    public class ImageClient
    {
        private const string GroupImagePath = "sb/v1/content/groups/image";

        public Task<byte[]> FetchGroupImage()
        {
            return http.GetByteArrayAsync("/sb/v1/content/groups/image");
        }

        public Task<byte[]> FetchLargeImage()
        {
            return http.GetByteArrayAsync("/sb/v1/content/groups/image2");
        }

        public Task<byte[]> FetchGroup(int id)
        {
            return http.GetByteArrayAsync($"/sb/v1/content/groups/{id}");
        }
    }

    public class GalleryService
    {
        public void Refresh()
        {
            var client = new ImageClient();
            client.FetchGroupImage();
        }
    }
}
