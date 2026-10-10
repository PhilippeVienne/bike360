# Environnement de production. À compléter avant le premier déploiement (voir docs/deploiement-aws.md).
env     = "production"
protect = true

# Nom de domaine du site. S'il est géré dans Route 53, donner aussi l'identifiant de sa zone :
# les enregistrements (certificat, site, signature des courriels) sont alors créés tout seuls.
domain  = "" # À COMPLÉTER : bike360.exemple
zone_id = ""
# Hors Route 53 : laisser à false au premier passage, créer les enregistrements donnés en sortie
# (terraform output dns), puis passer à true et relancer.
certificate_ready = false

# Expéditeur des courriels du service (rappel de renouvellement, échec de paiement, originaux prêts).
mail_from = "" # À COMPLÉTER : Bike360 <bonjour@bike360.exemple>

# Identité du vendeur portée sur les reçus : nom, adresse, SIREN, séparés par des virgules.
seller = "" # À COMPLÉTER

alert_email = "" # À COMPLÉTER
budget_usd  = 100
