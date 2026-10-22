# Payments runbook

## Failover

Όταν ο πάροχος πληρωμών δεν απαντά για περισσότερο από δύο λεπτά, το
σύστημα μεταβαίνει αυτόματα στον εφεδρικό πάροχο. Run `payments-cli
failover --to secondary` only if the automatic switch did not happen.

## Settlement

Settlement files arrive at 06:00 UTC. Αν το αρχείο καθυστερήσει πάνω από
μία ώρα, ειδοποιείται η ομάδα οικονομικών.
