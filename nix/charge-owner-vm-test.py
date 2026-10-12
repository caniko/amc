with test_section(
    "mixed original swap charges and offline fallback retain native backing"
):
    print(machine.succeed("python3 /etc/charge-owner-proof.py", timeout=120))
    evidence["chargeOwner"] = json.loads(
        machine.succeed("cat /tmp/charge-owner-evidence.json")
    )
