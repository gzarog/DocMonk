namespace Shop.Content
{
    [ApiController]
    [Route("sb/v1")]
    public class ContentController : ControllerBase
    {
        [HttpGet("content/groups/image")]
        public IActionResult GetGroupImage()
        {
            return Ok();
        }

        [HttpGet("content/groups/{id}")]
        public IActionResult GetGroup(int id)
        {
            return Ok();
        }
    }
}
