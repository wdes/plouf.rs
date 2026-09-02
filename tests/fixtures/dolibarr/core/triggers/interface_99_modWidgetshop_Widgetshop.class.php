<?php
// A synthetic Dolibarr trigger handler.
require_once DOL_DOCUMENT_ROOT.'/core/triggers/dolibarrtriggers.class.php';

class InterfaceWidgetshop extends DolibarrTriggers
{
    public function runTrigger($action, $object, User $user, Translate $langs, Conf $conf)
    {
        switch ($action) {
            case 'WIDGET_VALIDATE':
                // React to a validated widget.
                break;
        }
        return 0;
    }
}
