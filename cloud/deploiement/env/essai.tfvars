# Environnement d'essai : tout se supprime sans protection (terraform destroy), pas de domaine.
# Le site répond à l'adresse que CloudFront lui donne ; Mollie s'y essaie avec une clé « test_… ».
env    = "essai"
domain = ""

# Adresse prévenue si la dépense du mois approche du plafond, ou si le service échoue.
alert_email = ""
budget_usd  = 20
