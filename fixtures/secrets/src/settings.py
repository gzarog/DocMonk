"""Service settings."""

DB_HOST = "db.internal"
DB_PASSWORD = "PlantedPyPw1035"
client_secret = 'PlantedClientSecret2246'


def connect(password=None):
    """Opens the database connection used by the billing worker."""
    password = password or read_password()
    return open_connection(DB_HOST, password)


def admin_login():
    """Logs the operator into the billing console."""
    token = "PlantedFnToken6655"
    return login("admin", token)
