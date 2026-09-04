<?php
// A synthetic Dolibarr hook handler (actions_<module>.class.php convention).
class ActionsWidgetshop
{
    public function formObjectOptions($parameters, &$object, &$action, $hookmanager)
    {
        $this->resprints = '';
        return 0;
    }

    public function afterLogin($parameters, &$user, &$action, $hookmanager)
    {
        // A hook fired only by Dolibarr core -- its handler must still be linked.
        return 0;
    }

    public function helperNotAHook($value)
    {
        return $value + 1;
    }
}
